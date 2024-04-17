//! Wrapper crate over the jsonrpsee ws client, which automatically reconnects
//! under the hood; without that, the user has to restart it manually by
//! re-transmitting pending calls and re-establish subscriptions that
//! were closed when the connection was terminated.
//!
//! The tricky part is subscription, which may lose a few notifications,
//! then re-connect where it's not possible to know which ones.
//!
//! Lost subscription notifications may be very important to know in some cases,
//! and then this library is not recommended to use.
//!
//! # Examples
//!
//! ```rust
//!    use std::time::Duration;
//!    use reconnecting_jsonrpsee_ws_client::{Client, ExponentialBackoff, PingConfig, rpc_params};
//!
//!    async fn run() {
//!        // Create a new client with with a reconnecting RPC client.
//!        let client = Client::builder()
//!             // Reconnect with exponential backoff.
//!            .retry_policy(ExponentialBackoff::from_millis(100))
//!            // Send period WebSocket pings/pongs every 6th second and if it's not ACK:ed in 30 seconds
//!            // then disconnect.
//!            //
//!            // This is just a way to ensure that the connection isn't idle if no message is sent that often
//!            .enable_ws_ping(
//!                PingConfig::new()
//!                .ping_interval(Duration::from_secs(6))
//!                .inactive_limit(Duration::from_secs(30)),
//!            )
//!            // There are other configurations as well that can be found here:
//!            // <https://docs.rs/reconnecting-jsonrpsee-ws-client/latest/reconnecting_jsonrpsee_ws_client/struct.ClientBuilder.html>
//!            .build("ws://localhost:9944".to_string())
//!            .await.unwrap();
//!
//!        // make a JSON-RPC call
//!        let json = client.request("say_hello".to_string(), rpc_params![]).await.unwrap();
//!
//!        // make JSON-RPC subscription.
//!        let mut sub = client.subscribe("subscribe_lo".to_string(), rpc_params![], "unsubscribe_lo".to_string()).await.unwrap();
//!        let notif = sub.next().await.unwrap();
//!    }
//! ```
#![warn(missing_docs)]

#[cfg(any(
    all(feature = "web", feature = "native"),
    not(any(feature = "web", feature = "native"))
))]
compile_error!(
    "reconnecting-jsonrpsee-client: exactly one of the 'web' and 'native' features should be used."
);

mod platform;
mod utils;

use crate::utils::display_close_reason;
use finito::Retry;
use futures::{future::BoxFuture, FutureExt, Stream, StreamExt};
use jsonrpsee::core::{
    client::{
        Client as WsClient, ClientT, IdKind, Subscription as RpcSubscription, SubscriptionClientT,
        SubscriptionKind,
    },
    traits::ToRpcParams,
};
#[cfg(all(feature = "native", not(feature = "web")))]
use jsonrpsee::ws_client::HeaderMap;
use serde_json::value::RawValue;
use std::{
    collections::HashMap,
    pin::Pin,
    sync::Arc,
    task::{self, Poll},
    time::Duration,
};
use tokio::sync::{
    mpsc::{self, UnboundedReceiver, UnboundedSender},
    oneshot,
};
use utils::{reconnect_channel, MaybePendingFutures, ReconnectRx, ReconnectTx};

// re-exports
pub use finito::{ExponentialBackoff, FibonacciBackoff, FixedInterval};
#[cfg(all(feature = "native", not(feature = "web")))]
pub use jsonrpsee::core::client::async_client::PingConfig;
pub use jsonrpsee::{core::client::error::Error as RpcError, rpc_params, types::SubscriptionId};

const LOG_TARGET: &str = "reconnecting_jsonrpsee_ws_client";

/// Method result.
pub type MethodResult = Result<Box<RawValue>, RpcError>;
/// Subscription result.
pub type SubscriptionResult = Result<Box<RawValue>, Disconnect>;

/// Serialized JSON-RPC params.
#[derive(Debug, Clone)]
pub struct RpcParams(Option<Box<RawValue>>);

impl RpcParams {
    /// Create new [`RpcParams`] from JSON.
    pub fn new(json: Option<Box<RawValue>>) -> Self {
        Self(json)
    }
}

impl ToRpcParams for RpcParams {
    fn to_rpc_params(self) -> Result<Option<Box<RawValue>>, serde_json::Error> {
        Ok(self.0)
    }
}

/// How to handle when a subscription or method call when the connection was closed.
///
/// In some scenarios subscription may have "side-effects" and re-subscriptions
/// may not the case to handle it.
#[derive(Debug, Copy, Clone)]
pub enum CallRetryPolicy {
    /// When the connection is lost the call is re-tried.
    Retry,
    /// When the connection is lost the call is dropped.
    Drop,
}

/// An error that indicates the subscription
/// was disconnnected and will automatically reconnect.
#[derive(Debug, thiserror::Error)]
pub enum Disconnect {
    /// The connection was closed, reconnect initiated and the subscriptin was re-subscribed to.
    #[error("The client was disconnected `{0}`, reconnect and re-subscribe initiated")]
    Retry(RpcError),
    /// The connection was closed, reconnect initated and the subscription was dropped.
    #[error("The client was disconnected `{0}`, reconnect initiated and subscription dropped")]
    Dropped(RpcError),
}

/// Represent a single subscription.
pub struct Subscription {
    id: SubscriptionId<'static>,
    stream: mpsc::UnboundedReceiver<SubscriptionResult>,
}

impl Subscription {
    /// Returns the next notification from the stream.
    /// This may return `None` if the subscription has been terminated,
    /// which may happen if the channel becomes full or is dropped.
    ///
    /// **Note:** This has an identical signature to the [`StreamExt::next`]
    /// method (and delegates to that). Import [`StreamExt`] if you'd like
    /// access to other stream combinator methods.
    #[allow(clippy::should_implement_trait)]
    pub async fn next(&mut self) -> Option<SubscriptionResult> {
        StreamExt::next(self).await
    }

    /// Get the subscription ID.
    pub fn id(&self) -> SubscriptionId<'static> {
        self.id.clone()
    }
}

impl Stream for Subscription {
    type Item = SubscriptionResult;

    fn poll_next(
        mut self: Pin<&mut Self>,
        cx: &mut task::Context<'_>,
    ) -> task::Poll<Option<Self::Item>> {
        match self.stream.poll_recv(cx) {
            Poll::Ready(Some(msg)) => Poll::Ready(Some(msg)),
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl std::fmt::Debug for Subscription {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_fmt(format_args!("Subscription {:?}", self.id))
    }
}

/// JSON-RPC client that reconnects automatically and may loose
/// subscription notifications when it reconnects.
#[derive(Clone)]
pub struct Client {
    tx: mpsc::UnboundedSender<Op>,
    reconnect: ReconnectRx,
}

/// Builder for [`Client`].
#[derive(Clone)]
pub struct ClientBuilder<P> {
    max_request_size: u32,
    max_response_size: u32,
    retry_policy: P,
    #[cfg(all(feature = "native", not(feature = "web")))]
    ping_config: Option<PingConfig>,
    #[cfg(all(feature = "native", not(feature = "web")))]
    // web doesn't support custom headers
    // https://stackoverflow.com/a/4361358/6394734
    headers: HeaderMap,
    max_redirections: u32,
    id_kind: IdKind,
    max_log_len: u32,
    max_concurrent_requests: u32,
    request_timeout: Duration,
    connection_timeout: Duration,
}

impl Default for ClientBuilder<ExponentialBackoff> {
    fn default() -> Self {
        Self {
            max_request_size: 10 * 1024 * 1024,
            max_response_size: 10 * 1024 * 1024,
            retry_policy: ExponentialBackoff::from_millis(10),
            #[cfg(all(feature = "native", not(feature = "web")))]
            ping_config: Some(PingConfig::new()),
            #[cfg(all(feature = "native", not(feature = "web")))]
            headers: HeaderMap::new(),
            max_redirections: 5,
            id_kind: IdKind::Number,
            max_log_len: 1024,
            max_concurrent_requests: 1024,
            request_timeout: Duration::from_secs(60),
            connection_timeout: Duration::from_secs(10),
        }
    }
}

impl ClientBuilder<ExponentialBackoff> {
    /// Create a new builder.
    pub fn new() -> Self {
        Self::default()
    }
}

impl<P> ClientBuilder<P>
where
    P: Iterator<Item = Duration> + Send + Sync + 'static + Clone,
{
    /// Configure the min response size a for websocket message.
    ///
    /// Default: 10MB
    pub fn max_request_size(mut self, max: u32) -> Self {
        self.max_request_size = max;
        self
    }

    /// Configure the max response size a for websocket message.
    ///
    /// Default: 10MB
    pub fn max_response_size(mut self, max: u32) -> Self {
        self.max_response_size = max;
        self
    }

    /// Set the max number of redirections to perform until a connection is regarded as failed.
    ///
    /// Default: 5
    pub fn max_redirections(mut self, redirect: u32) -> Self {
        self.max_redirections = redirect;
        self
    }

    /// Configure how many concurrent method calls are allowed.
    ///
    /// Default: 1024
    pub fn max_concurrent_requests(mut self, max: u32) -> Self {
        self.max_concurrent_requests = max;
        self
    }

    /// Configure how long until a method call is regarded as failed.
    ///
    /// Default: 1 minute
    pub fn request_timeout(mut self, timeout: Duration) -> Self {
        self.request_timeout = timeout;
        self
    }

    /// Set connection timeout for the WebSocket handshake
    ///
    /// Default: 10 seconds
    pub fn connection_timeout(mut self, timeout: Duration) -> Self {
        self.connection_timeout = timeout;
        self
    }

    /// Configure the data type of the request object ID
    ///
    /// Default: number
    pub fn id_format(mut self, kind: IdKind) -> Self {
        self.id_kind = kind;
        self
    }

    /// Set maximum length for logging calls and responses.
    /// Logs bigger than this limit will be truncated.
    ///
    /// Default: 1024
    pub fn set_max_logging_length(mut self, max: u32) -> Self {
        self.max_log_len = max;
        self
    }

    #[cfg(all(feature = "native", not(feature = "web")))]
    /// Configure custom headers to use in the WebSocket handshake.
    pub fn set_headers(mut self, headers: HeaderMap) -> Self {
        self.headers = headers;
        self
    }

    /// Configure which retry policy to use when a connection is lost.
    ///
    /// Default: Exponential backoff 10ms
    pub fn retry_policy<T>(self, retry_policy: T) -> ClientBuilder<T> {
        ClientBuilder {
            max_request_size: self.max_request_size,
            max_response_size: self.max_response_size,
            retry_policy,
            #[cfg(all(feature = "native", not(feature = "web")))]
            ping_config: self.ping_config,
            #[cfg(all(feature = "native", not(feature = "web")))]
            headers: self.headers,
            max_redirections: self.max_redirections,
            max_log_len: self.max_log_len,
            id_kind: self.id_kind,
            max_concurrent_requests: self.max_concurrent_requests,
            request_timeout: self.request_timeout,
            connection_timeout: self.connection_timeout,
        }
    }

    #[cfg(all(feature = "native", not(feature = "web")))]
    /// Configure the WebSocket ping/pong interval.
    ///
    /// Default: 30 seconds.
    pub fn enable_ws_ping(mut self, ping_config: PingConfig) -> Self {
        self.ping_config = Some(ping_config);
        self
    }

    #[cfg(all(feature = "native", not(feature = "web")))]
    /// Disable WebSocket ping/pongs.
    ///
    /// Default: 30 seconds.
    pub fn disable_ws_ping(mut self) -> Self {
        self.ping_config = None;
        self
    }

    /// Build and connect to the target.
    pub async fn build(self, url: String) -> Result<Client, RpcError> {
        let (tx, rx) = mpsc::unbounded_channel();
        let client = Retry::new(self.retry_policy.clone(), || {
            platform::ws_client(url.as_ref(), &self)
        })
        .await?;
        let (reconn_tx, reconn_rx) = reconnect_channel();

        platform::spawn(background_task(client, rx, url, reconn_tx, self));

        Ok(Client {
            tx,
            reconnect: reconn_rx,
        })
    }
}

impl Client {
    /// Create a method call.
    pub async fn request<P: ToRpcParams>(
        &self,
        method: String,
        params: P,
    ) -> Result<Box<RawValue>, RpcError> {
        let params = params.to_rpc_params()?;
        self.request_raw(method, params).await
    }

    /// Create a method call.
    pub async fn request_with_policy<P: ToRpcParams>(
        &self,
        method: String,
        params: P,
        policy: CallRetryPolicy,
    ) -> Result<Box<RawValue>, RpcError> {
        let params = params.to_rpc_params()?;
        self.request_raw_with_policy(method, params, policy).await
    }

    /// Similar to [`Client::request`] but doesn't check
    /// that the params are valid JSON-RPC params.
    pub async fn request_raw_with_policy(
        &self,
        method: String,
        params: Option<Box<RawValue>>,
        policy: CallRetryPolicy,
    ) -> Result<Box<RawValue>, RpcError> {
        let (tx, rx) = oneshot::channel();

        self.tx
            .send(Op::Call {
                method,
                params: RpcParams::new(params),
                send_back: tx,
                policy,
            })
            .map_err(|_| RpcError::Custom("Client is dropped".to_string()))?;
        rx.await
            .map_err(|_| RpcError::Custom("Client is dropped".to_string()))?
    }

    /// Similar to [`Client::request`] but doesn't check
    /// that the params are valid JSON-RPC params.
    pub async fn request_raw(
        &self,
        method: String,
        params: Option<Box<RawValue>>,
    ) -> Result<Box<RawValue>, RpcError> {
        self.request_raw_with_policy(method, params, CallRetryPolicy::Retry)
            .await
    }

    /// Create a subscription which doesn't re-subscribe if the connection was lost.
    pub async fn subscribe_with_policy<P: ToRpcParams>(
        &self,
        subscribe_method: String,
        params: P,
        unsubscribe_method: String,
        policy: CallRetryPolicy,
    ) -> Result<Subscription, RpcError> {
        let params = params.to_rpc_params()?;
        self.subscribe_raw_with_policy(subscribe_method, params, unsubscribe_method, policy)
            .await
    }

    /// Create a subscription.
    pub async fn subscribe<P: ToRpcParams>(
        &self,
        subscribe_method: String,
        params: P,
        unsubscribe_method: String,
    ) -> Result<Subscription, RpcError> {
        let params = params.to_rpc_params()?;
        self.subscribe_raw(subscribe_method, params, unsubscribe_method)
            .await
    }

    /// Similar to [`Client::subscribe`] but doesn't check
    /// that the params are valid JSON-RPC params.
    pub async fn subscribe_raw(
        &self,
        subscribe_method: String,
        params: Option<Box<RawValue>>,
        unsubscribe_method: String,
    ) -> Result<Subscription, RpcError> {
        self.subscribe_raw_with_policy(
            subscribe_method,
            params,
            unsubscribe_method,
            CallRetryPolicy::Retry,
        )
        .await
    }

    /// Similar to [`Client::subscribe_raw`] but allows to decide whether to re-subscribe when
    /// the connection is closed.
    pub async fn subscribe_raw_with_policy(
        &self,
        subscribe_method: String,
        params: Option<Box<RawValue>>,
        unsubscribe_method: String,
        policy: CallRetryPolicy,
    ) -> Result<Subscription, RpcError> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(Op::Subscription {
                subscribe_method,
                params: RpcParams::new(params),
                unsubscribe_method,
                send_back: tx,
                policy,
            })
            .map_err(|_| RpcError::Custom("Client is dropped".to_string()))?;
        rx.await
            .map_err(|_| RpcError::Custom("Client is dropped".to_string()))?
    }

    /// A future that returns once the client starts to reconnect
    /// and it doesn't mean that a new connection was established yet.
    ///
    /// This may be called multiple times.
    pub async fn on_reconnect(&self) {
        self.reconnect.on_reconnect().await
    }

    /// Get how many times the client has reconnected.
    pub fn reconnect_count(&self) -> usize {
        self.reconnect.count()
    }
}

impl Client {
    /// Create a builder.
    pub fn builder() -> ClientBuilder<ExponentialBackoff> {
        ClientBuilder::new()
    }
}

#[derive(Debug)]
enum Op {
    Call {
        method: String,
        params: RpcParams,
        send_back: oneshot::Sender<MethodResult>,
        policy: CallRetryPolicy,
    },
    Subscription {
        subscribe_method: String,
        params: RpcParams,
        unsubscribe_method: String,
        send_back: oneshot::Sender<Result<Subscription, RpcError>>,
        policy: CallRetryPolicy,
    },
}

#[derive(Debug)]
struct RetrySubscription {
    tx: mpsc::UnboundedSender<SubscriptionResult>,
    subscribe_method: String,
    params: RpcParams,
    unsubscribe_method: String,
    policy: CallRetryPolicy,
}

#[derive(Debug)]
enum Closed {
    Dropped,
    Retry { op: Op, id: usize },
}

async fn background_task<P>(
    mut client: Arc<WsClient>,
    mut rx: UnboundedReceiver<Op>,
    url: String,
    reconn: ReconnectTx,
    client_builder: ClientBuilder<P>,
) where
    P: Iterator<Item = Duration> + Send + 'static + Clone,
{
    let (sub_tx, mut sub_rx) = mpsc::unbounded_channel();
    let mut pending_calls = MaybePendingFutures::new();
    let mut open_subscriptions = HashMap::new();
    let mut id = 0;

    loop {
        tracing::trace!(
            target: LOG_TARGET,
            "pending_calls={} open_subscriptions={}, client_restarts={}",
            pending_calls.len(),
            open_subscriptions.len(),
            reconn.count(),
        );

        tokio::select! {
            // An incoming JSON-RPC call to dispatch.
            next_message = rx.recv() => {
                match next_message {
                    None => break,
                    Some(op) => {
                        pending_calls.push(dispatch_call(client.clone(), op, id, sub_tx.clone()).boxed());
                    }
                };
            }
            // Reponse to JSON-RPC ready.
            next_response = pending_calls.next() => {
                match next_response {
                    // New subscription opened.
                    Some(Ok(DispatchedCall::Subscription { id, sub })) => {
                        open_subscriptions.insert(id, sub);
                    }
                    // The connection was closed, re-connect and try to send all messages again.
                    Some(Err(Closed::Retry { op, id })) => {
                        let params = ReconnectParams {
                            url: &url,
                            pending_calls: &mut pending_calls,
                            dispatch: vec![(id, op)],
                            reconnect: reconn.clone(),
                            sub_tx: sub_tx.clone(),
                            open_subscriptions: &open_subscriptions,
                            client_builder: &client_builder,
                            close_reason: client.disconnect_reason().await,
                        };

                        client = match reconnect(params).await {
                            Ok(client) => client,
                            Err(e) => {
                                tracing::error!(target: LOG_TARGET, "Failed to reconnect/re-establish subscriptions: {e}; terminating the connection");
                                break;
                            }
                        };
                    }
                    // Method call dispatched.
                    Some(Err(Closed::Dropped)) | Some(Ok(DispatchedCall::Done)) => (),
                    None => break,
                }
            }

            // The connection was terminated and try to reconnect.
            _ = client.on_disconnect() => {
                let params = ReconnectParams {
                    url: &url,
                    pending_calls: &mut pending_calls,
                    dispatch: vec![],
                    reconnect: reconn.clone(),
                    sub_tx: sub_tx.clone(),
                    open_subscriptions: &open_subscriptions,
                    client_builder: &client_builder,
                    close_reason: client.disconnect_reason().await,
                };

                client = match reconnect(params).await {
                    Ok(client) => client,
                    Err(e) => {
                        tracing::error!(target: LOG_TARGET, "Failed to reconnect/re-establish subscriptions: {e}; terminating the connection");
                        break;
                    }
                };
            }

            // Subscription was closed
            maybe_sub_closed = sub_rx.recv() => {
                let Some(id) = maybe_sub_closed else {
                    break
                };
                open_subscriptions.remove(&id);
            }
        }
        id = id.wrapping_add(1);
    }
}

/// The outcome of dispatched call
/// which may be call or a subscription.
enum DispatchedCall {
    Done,
    Subscription { id: usize, sub: RetrySubscription },
}

async fn dispatch_call(
    client: Arc<WsClient>,
    op: Op,
    id: usize,
    remove_sub: mpsc::UnboundedSender<usize>,
) -> Result<DispatchedCall, Closed> {
    match op {
        Op::Call {
            method,
            params,
            send_back,
            policy,
        } => {
            match client
                .request::<Box<RawValue>, _>(&method, params.clone())
                .await
            {
                Ok(rp) => {
                    // Fails only if the request is dropped.
                    let _ = send_back.send(Ok(rp));
                    Ok(DispatchedCall::Done)
                }
                Err(RpcError::RestartNeeded(e)) => {
                    if matches!(policy, CallRetryPolicy::Drop) {
                        let _ = send_back.send(Err(RpcError::RestartNeeded(e)));
                        Err(Closed::Dropped)
                    } else {
                        Err(Closed::Retry {
                            op: Op::Call {
                                method,
                                params,
                                send_back,
                                policy,
                            },
                            id,
                        })
                    }
                }
                Err(e) => {
                    // Fails only if the request is dropped.
                    let _ = send_back.send(Err(e));
                    Ok(DispatchedCall::Done)
                }
            }
        }
        Op::Subscription {
            subscribe_method,
            params,
            unsubscribe_method,
            send_back,
            policy,
        } => {
            match client
                .subscribe::<Box<RawValue>, _>(
                    &subscribe_method,
                    params.clone(),
                    &unsubscribe_method,
                )
                .await
            {
                Ok(sub) => {
                    let (tx, rx) = mpsc::unbounded_channel();
                    let sub_id = match sub.kind() {
                        SubscriptionKind::Subscription(id) => id.clone().into_owned(),
                        _ => unreachable!("No method subscriptions possible in this crate"),
                    };

                    platform::spawn(subscription_handler(
                        tx.clone(),
                        sub,
                        remove_sub,
                        id,
                        client.clone(),
                        policy,
                    ));

                    let sub = RetrySubscription {
                        tx,
                        subscribe_method,
                        params,
                        unsubscribe_method,
                        policy,
                    };

                    let stream = Subscription {
                        id: sub_id,
                        stream: rx,
                    };

                    // Fails only if the request is dropped.
                    let _ = send_back.send(Ok(stream));
                    Ok(DispatchedCall::Subscription { id, sub })
                }
                Err(RpcError::RestartNeeded(e)) => {
                    if matches!(policy, CallRetryPolicy::Drop) {
                        let _ = send_back.send(Err(RpcError::RestartNeeded(e)));
                        Err(Closed::Dropped)
                    } else {
                        Err(Closed::Retry {
                            op: Op::Subscription {
                                subscribe_method,
                                params,
                                unsubscribe_method,
                                send_back,
                                policy,
                            },
                            id,
                        })
                    }
                }
                Err(e) => {
                    // Fails only if the request is dropped.
                    let _ = send_back.send(Err(e));
                    Ok(DispatchedCall::Done)
                }
            }
        }
    }
}

/// Handler for each individual subscription.
async fn subscription_handler(
    sub_tx: UnboundedSender<SubscriptionResult>,
    mut rpc_sub: RpcSubscription<Box<RawValue>>,
    remove_sub: mpsc::UnboundedSender<usize>,
    id: usize,
    client: Arc<WsClient>,
    policy: CallRetryPolicy,
) {
    let drop = loop {
        tokio::select! {
            next_msg = rpc_sub.next() => {
                let Some(notif) = next_msg else {
                    let close = client.disconnect_reason().await;

                    let drop = if matches!(policy, CallRetryPolicy::Retry) {
                        sub_tx.send(Err(Disconnect::Retry(close))).is_err()
                    } else {
                        let _ = sub_tx.send(Err(Disconnect::Dropped(close)));
                        true
                    };

                    break drop
                };

                let msg = notif.expect("RawValue is valid JSON; qed");

                // Fails only if subscription was closed by the user.
                if sub_tx.send(Ok(msg)).is_err() {
                    break true
                }
            }
             // This channel indices whether the subscription was closed by user.
             _ = sub_tx.closed() => {
                break true
            }
            // This channel indices wheter the main task has been closed.
            // at this point no further messages are processed.
            _ = remove_sub.closed() => {
                break true
            }
        }
    };

    // The subscription was dropped.
    // Thus, the subscription should be removed.
    if drop {
        let _ = remove_sub.send(id);
    }
}

struct ReconnectParams<'a, P> {
    url: &'a str,
    pending_calls: &'a mut MaybePendingFutures<BoxFuture<'static, Result<DispatchedCall, Closed>>>,
    dispatch: Vec<(usize, Op)>,
    reconnect: ReconnectTx,
    sub_tx: UnboundedSender<usize>,
    open_subscriptions: &'a HashMap<usize, RetrySubscription>,
    client_builder: &'a ClientBuilder<P>,
    close_reason: RpcError,
}

async fn reconnect<P>(params: ReconnectParams<'_, P>) -> Result<Arc<WsClient>, RpcError>
where
    P: Iterator<Item = Duration> + Send + 'static + Clone,
{
    let ReconnectParams {
        url,
        pending_calls,
        mut dispatch,
        reconnect,
        sub_tx,
        open_subscriptions,
        client_builder,
        close_reason,
    } = params;

    let retry_policy = client_builder.retry_policy.clone();

    // All futures should return now because the connection has been terminated.
    while !pending_calls.is_empty() {
        match pending_calls.next().await {
            Some(Ok(_)) | None | Some(Err(Closed::Dropped)) => {}
            Some(Err(Closed::Retry { op, id })) => {
                dispatch.push((id, op));
            }
        };
    }

    tracing::debug!(target: LOG_TARGET, "Connection to {url} was closed: `{}`", display_close_reason(&close_reason));

    reconnect.reconnect();
    let client = Retry::new(retry_policy.clone(), || {
        platform::ws_client(url, client_builder)
    })
    .await?;

    for (id, op) in dispatch {
        pending_calls.push(dispatch_call(client.clone(), op, id, sub_tx.clone()).boxed());
    }

    for (id, s) in open_subscriptions.iter() {
        if matches!(s.policy, CallRetryPolicy::Drop) {
            continue;
        }

        let sub = Retry::new(retry_policy.clone(), || {
            client.subscribe::<Box<RawValue>, _>(
                &s.subscribe_method,
                s.params.clone(),
                &s.unsubscribe_method,
            )
        })
        .await?;

        platform::spawn(subscription_handler(
            s.tx.clone(),
            sub,
            sub_tx.clone(),
            *id,
            client.clone(),
            s.policy,
        ));
    }

    Ok(client)
}

#[cfg(test)]
mod tests {
    use std::{convert::Infallible, net::TcpListener};

    use super::*;
    use futures::{
        future::{self, Either},
        stream::FuturesUnordered,
        TryStreamExt,
    };
    use hyper::server::conn::AddrStream;
    use jsonrpsee::{
        rpc_params,
        server::{
            http, stop_channel, ws, ConnectionGuard, ConnectionState, RpcServiceBuilder,
            ServerConfig,
        },
        RpcModule, SubscriptionMessage,
    };
    use tracing_subscriber::util::SubscriberInitExt;

    fn init_logger() {
        let filter = tracing_subscriber::EnvFilter::from_default_env();
        let _ = tracing_subscriber::fmt()
            .with_env_filter(filter)
            .finish()
            .try_init();
    }

    #[tokio::test]
    async fn call_works() {
        init_logger();
        let (_handle, addr) = run_server().await.unwrap();

        let client = Client::builder().build(addr).await.unwrap();

        assert!(client
            .request("say_hello".to_string(), rpc_params![])
            .await
            .is_ok(),)
    }

    #[tokio::test]
    async fn sub_works() {
        init_logger();
        let (_handle, addr) = run_server().await.unwrap();

        let client = Client::builder()
            .retry_policy(ExponentialBackoff::from_millis(50))
            .build(addr)
            .await
            .unwrap();

        let mut sub = client
            .subscribe(
                "subscribe_lo".to_string(),
                rpc_params![],
                "unsubscribe_lo".to_string(),
            )
            .await
            .unwrap();

        assert!(sub.next().await.is_some());
    }

    #[tokio::test]
    async fn reconn_sub_works() {
        init_logger();
        let (handle, addr) = run_server().await.unwrap();
        let client = Client::builder().build(addr.clone()).await.unwrap();

        let mut sub = client
            .subscribe(
                "subscribe_lo".to_string(),
                rpc_params![],
                "unsubscribe_lo".to_string(),
            )
            .await
            .unwrap();

        let _ = handle.send(());
        client.on_reconnect().await;

        // Restart the server.
        let (_handle, _) = run_server_with_settings(Some(&addr), false).await.unwrap();

        // Ensure that the client reconnects and that subscription keep running when
        // the connection is established again.
        for _ in 0..10 {
            assert!(sub.next().await.is_some());
        }

        assert_eq!(client.reconnect_count(), 1);
    }

    #[tokio::test]
    async fn reconn_sub_drop_policy_works() {
        init_logger();
        let (handle, addr) = run_server().await.unwrap();
        let client = Client::builder().build(addr.clone()).await.unwrap();

        let mut sub = client
            .subscribe_with_policy(
                "subscribe_lo".to_string(),
                rpc_params![],
                "unsubscribe_lo".to_string(),
                CallRetryPolicy::Drop,
            )
            .await
            .unwrap();

        let _ = handle.send(());
        client.on_reconnect().await;

        // Restart the server.
        let (_handle, _) = run_server_with_settings(Some(&addr), false).await.unwrap();

        // Ensure that the client reconnects and that subscription keep running when
        // the connection is established again.
        assert!(sub.next().await.is_some());
        assert!(matches!(
            sub.next().await,
            Some(Err(Disconnect::Dropped(_)))
        ));
    }

    #[tokio::test]
    async fn sub_emits_reconn_notif() {
        init_logger();
        let (handle, addr) = run_server().await.unwrap();
        let client = Client::builder().build(addr.clone()).await.unwrap();

        let mut sub = client
            .subscribe(
                "subscribe_lo".to_string(),
                rpc_params![],
                "unsubscribe_lo".to_string(),
            )
            .await
            .unwrap();

        let _ = handle.send(());
        client.on_reconnect().await;

        assert!(matches!(sub.next().await, Some(Ok(_))));
        assert!(matches!(sub.next().await, Some(Err(Disconnect::Retry(_)))));

        assert_eq!(client.reconnect_count(), 1);
    }

    #[tokio::test]
    async fn reconn_calls_works() {
        init_logger();
        let (handle, addr) = run_server_with_settings(None, true).await.unwrap();

        let client = Arc::new(Client::builder().build(addr.clone()).await.unwrap());

        let req_fut = client
            .request("say_hello".to_string(), rpc_params![])
            .boxed();
        let timeout_fut = tokio::time::sleep(Duration::from_secs(5));

        // If the call isn't replied in 5 secs then it's regarded as it's still pending.
        let req_fut = match futures::future::select(Box::pin(timeout_fut), req_fut).await {
            Either::Left((_, f)) => f,
            Either::Right(_) => panic!("RPC call finished"),
        };

        let _ = handle.send(());
        client.on_reconnect().await;

        // Restart the server and allow the call to complete.
        let (_handle, _) = run_server_with_settings(Some(&addr), false).await.unwrap();

        assert!(req_fut.await.is_ok());
    }

    #[tokio::test]
    async fn reconn_call_with_policy_works() {
        init_logger();
        let (handle, addr) = run_server_with_settings(None, true).await.unwrap();

        let client = Arc::new(Client::builder().build(addr.clone()).await.unwrap());

        let req_fut = client
            .request_with_policy(
                "say_hello".to_string(),
                rpc_params![],
                CallRetryPolicy::Drop,
            )
            .boxed();
        let timeout_fut = tokio::time::sleep(Duration::from_secs(5));

        // If the call isn't replied in 5 secs then it's regarded as it's still pending.
        let req_fut = match futures::future::select(Box::pin(timeout_fut), req_fut).await {
            Either::Left((_, f)) => f,
            Either::Right(rp) => panic!("RPC call finished rp={:?}", rp),
        };

        let _ = handle.send(());
        client.on_reconnect().await;

        // Restart the server and allow the call to complete.
        let (_handle, _) = run_server_with_settings(Some(&addr), false).await.unwrap();

        assert!(matches!(req_fut.await, Err(RpcError::RestartNeeded(_))));
    }

    #[tokio::test]
    async fn reconn_once_when_offline() {
        init_logger();
        let (handle, addr) = run_server_with_settings(None, true).await.unwrap();
        let client = Arc::new(Client::builder().build(addr.clone()).await.unwrap());

        let _ = handle.send(());
        client.on_reconnect().await;

        let client2 = client.clone();
        let reqs = tokio::spawn(async move {
            let futs: FuturesUnordered<_> = (0..10)
                .map(|_| client2.request("say_hello".to_string(), rpc_params![]))
                .collect();

            futs.try_for_each(|_| future::ready(Ok(())))
                .await
                .expect("Requests should be succesful");
        });

        // Restart the server and allow the call to complete.
        let (_handle, _) = run_server_with_settings(Some(&addr), false).await.unwrap();

        reqs.await.unwrap();

        assert_eq!(client.reconnect_count(), 1);
    }

    async fn run_server() -> anyhow::Result<(tokio::sync::broadcast::Sender<()>, String)> {
        run_server_with_settings(None, false).await
    }

    async fn run_server_with_settings(
        url: Option<&str>,
        dont_respond_to_method_calls: bool,
    ) -> anyhow::Result<(tokio::sync::broadcast::Sender<()>, String)> {
        use hyper::service::{make_service_fn, service_fn};

        let sockaddr = match url {
            Some(url) => url.strip_prefix("ws://").unwrap(),
            None => "127.0.0.1:0",
        };

        let mut i = 0;

        let listener = loop {
            if let Ok(l) = TcpListener::bind(sockaddr) {
                break l;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;

            if i >= 10 {
                panic!("Addr already in use");
            }

            i += 1;
        };

        let mut module = RpcModule::new(());

        if dont_respond_to_method_calls {
            module.register_async_method("say_hello", |_, _| async {
                futures::future::pending::<()>().await;
                "timeout"
            })?;
        } else {
            module.register_async_method("say_hello", |_, _| async { "lo" })?;
        }

        module.register_subscription(
            "subscribe_lo",
            "subscribe_lo",
            "unsubscribe_lo",
            |_params, pending, _ctx| async move {
                let sink = pending.accept().await.unwrap();
                let i = 0;

                loop {
                    if sink
                        .send(SubscriptionMessage::from_json(&i).unwrap())
                        .await
                        .is_err()
                    {
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_secs(6)).await;
                }
            },
        )?;

        let (tx, _) = tokio::sync::broadcast::channel(4);
        let tx2 = tx.clone();
        let (stop_handle, server_handle) = stop_channel();

        let make_service = make_service_fn(move |_: &AddrStream| {
            let module = module.clone();
            let tx = tx2.clone();
            let stop_handle = stop_handle.clone();

            async move {
                Ok::<_, Infallible>(service_fn(move |req| {
                    let module = module.clone();
                    let tx = tx.clone();
                    let stop_handle = stop_handle.clone();

                    let conn_permit = ConnectionGuard::new(1).try_acquire().unwrap();

                    if ws::is_upgrade_request(&req) {
                        let rpc_service = RpcServiceBuilder::new();
                        let conn = ConnectionState::new(stop_handle, 1, conn_permit);

                        async move {
                            let mut rx = tx.subscribe();

                            match ws::connect(
                                req,
                                ServerConfig::default(),
                                module,
                                conn,
                                rpc_service,
                            )
                            .await
                            {
                                Ok((rp, conn_fut)) => {
                                    tokio::spawn(async move {
                                        tokio::select! {
                                            _ = conn_fut => (),
                                            _ = rx.recv() => {},
                                        }
                                    });

                                    Ok::<_, Infallible>(rp)
                                }
                                Err(rp) => Ok(rp),
                            }
                        }
                        .boxed()
                    } else {
                        async { Ok(http::response::denied()) }.boxed()
                    }
                }))
            }
        });

        let addr = listener.local_addr()?;
        let server = hyper::Server::from_tcp(listener)?.serve(make_service);

        let mut rx = tx.subscribe();
        tokio::spawn(async move {
            let graceful = server.with_graceful_shutdown(async move {
                _ = rx.recv().await;
            });
            graceful.await.unwrap();
            drop(server_handle);
        });

        Ok((tx, format!("ws://{}", addr)))
    }
}
