//! # reconnecting-jsonrpsee-ws-client
//!
//! Wrapper crate over the jsonrpsee ws client, which automatically reconnects
//! under the hood; without that, one has to restart it.
//! It supports a few retry strategies, such as exponential backoff, but it's also possible
//! to use custom strategies as long as it implements `Iterator<Item = Duration>`.
//!
//!
//! By default, the library is re-transmitting pending calls and re-establishing subscriptions that
//! were closed until it's successful when the connection was terminated, but it's also possible to disable that
//! and manage it yourself.
//!
//! For instance, you may not want to re-subscribe to a subscription
//! that has side effects or retries at all. Then the library exposes
//! `request_with_policy` and `subscribe_with_policy` to support that
//!
//! ```no_run
//! async fn run() {
//!    use reconnecting_jsonrpsee_ws_client::{Client, CallRetryPolicy, rpc_params};
//!
//!    let client = Client::builder().build("ws://127.0.0.1:9944".to_string()).await.unwrap();
//!    let mut sub = client
//!        .subscribe_with_policy(
//!            "subscribe_lo".to_string(),
//!            rpc_params![],
//!            "unsubscribe_lo".to_string(),
//!            // Do not re-subscribe if the connection is closed.
//!            CallRetryPolicy::Retry,
//!        )
//!        .await
//!        .unwrap();
//! }
//! ```
//!
//!
//! The tricky part is subscriptions, which may lose a few notifications
//! when it's re-connecting, it's not possible to know which ones.
//!
//! Lost subscription notifications may be very important to know in some cases,
//! and then this library is not recommended to use.
//!
//!
//! There is one way to determine how long a reconnection takes:
//!
//! ```no_run
//! async fn run() {
//!     use reconnecting_jsonrpsee_ws_client::{Client, CallRetryPolicy, rpc_params};
//!
//!     let client = Client::builder().build("ws://127.0.0.1:9944".to_string()).await.unwrap();
//!    
//!     // Print when the RPC client starts to reconnect.
//!     tokio::spawn(async move {
//!        loop {
//!         client.reconnect_started().await;
//!         let now = std::time::Instant::now();
//!         client.reconnected().await;
//!         println!(
//!            "RPC client reconnection took `{} seconds`",
//!            now.elapsed().as_secs()
//!         );
//!        }
//!     });
//! }
//!
//! ```

#![warn(
    missing_docs,
    missing_debug_implementations,
    missing_copy_implementations
)]
#![cfg_attr(docsrs, feature(doc_cfg))]

#[cfg(not_supported)]
compile_error!(
    "reconnecting-jsonrpsee-client: exactly one of the 'web' and 'native' features most be used."
);

mod platform;
mod utils;

use crate::utils::display_close_reason;
use finito::Retry;
use futures::{future::BoxFuture, FutureExt, Stream, StreamExt};
use jsonrpsee::core::{
    client::{
        Client as WsClient, ClientT, Subscription as RpcSubscription, SubscriptionClientT,
        SubscriptionKind,
    },
    traits::ToRpcParams,
};
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
pub use jsonrpsee::core::client::IdKind;
pub use jsonrpsee::{core::client::error::Error as RpcError, rpc_params, types::SubscriptionId};

#[cfg(native)]
pub use jsonrpsee::ws_client::{HeaderMap, PingConfig};

const LOG_TARGET: &str = "reconnecting_jsonrpsee_ws_client";

/// Method result.
pub type MethodResult = Result<Box<RawValue>, Error>;
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
    /// When the connection is lost the call is dropped.
    Drop,
    /// When the connection is lost the call is re-tried but
    /// not re-subscribed if the subscription was established.
    Retry,
    /// Similar to Retry but also resubscribes the subscriptions that was
    /// active when the connection was closed.
    RetryAndResubscribe,
}

/// An error that indicates the subscription
/// was disconnected and may reconnect.
#[derive(Debug, thiserror::Error)]
pub enum Disconnect {
    /// The connection was closed, reconnect initiated and the subscription was re-subscribed to.
    #[error("The client was disconnected `{0}`, reconnect and re-subscribe initiated")]
    Retry(RpcError),
    /// The connection was closed, reconnect initiated and the subscription was dropped.
    #[error("The client was disconnected `{0}`, reconnect initiated and subscription dropped")]
    Dropped(RpcError),
}

/// Error that can occur when for a RPC call or subscription.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The client is closed.
    #[error("The client was disconnected")]
    Closed,
    /// The connection was closed and reconnect initiated.
    #[error("The client connection was closed and reconnect initiated")]
    DisconnectedWillReconnect,
    /// Other rpc error.
    #[error("{0}")]
    RpcError(RpcError),
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
#[derive(Clone, Debug)]
pub struct Client {
    tx: mpsc::UnboundedSender<Op>,
    reconnect: ReconnectRx,
}

/// Builder for [`Client`].
#[derive(Clone, Debug)]
pub struct ClientBuilder<P> {
    max_request_size: u32,
    max_response_size: u32,
    retry_policy: P,
    #[cfg(native)]
    ping_config: Option<PingConfig>,
    #[cfg(native)]
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
            retry_policy: ExponentialBackoff::from_millis(10).max_delay(Duration::from_secs(60)),
            #[cfg(native)]
            ping_config: Some(PingConfig::new()),
            #[cfg(native)]
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

    #[cfg(native)]
    #[cfg_attr(docsrs, doc(cfg(native)))]
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
            #[cfg(native)]
            ping_config: self.ping_config,
            #[cfg(native)]
            headers: self.headers,
            max_redirections: self.max_redirections,
            max_log_len: self.max_log_len,
            id_kind: self.id_kind,
            max_concurrent_requests: self.max_concurrent_requests,
            request_timeout: self.request_timeout,
            connection_timeout: self.connection_timeout,
        }
    }

    #[cfg(native)]
    #[cfg_attr(docsrs, doc(cfg(native)))]
    /// Configure the WebSocket ping/pong interval.
    ///
    /// Default: 30 seconds.
    pub fn enable_ws_ping(mut self, ping_config: PingConfig) -> Self {
        self.ping_config = Some(ping_config);
        self
    }

    #[cfg(native)]
    #[cfg_attr(docsrs, doc(cfg(native)))]
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
    ) -> Result<Box<RawValue>, Error> {
        let params = params
            .to_rpc_params()
            .map_err(|e| Error::RpcError(RpcError::ParseError(e)))?;
        self.request_raw(method, params).await
    }

    /// Create a method call.
    pub async fn request_with_policy<P: ToRpcParams>(
        &self,
        method: String,
        params: P,
        policy: CallRetryPolicy,
    ) -> Result<Box<RawValue>, Error> {
        let params = params
            .to_rpc_params()
            .map_err(|e| Error::RpcError(RpcError::ParseError(e)))?;
        self.request_raw_with_policy(method, params, policy).await
    }

    /// Similar to [`Client::request`] but doesn't check
    /// that the params are valid JSON-RPC params.
    pub async fn request_raw_with_policy(
        &self,
        method: String,
        params: Option<Box<RawValue>>,
        policy: CallRetryPolicy,
    ) -> Result<Box<RawValue>, Error> {
        let (tx, rx) = oneshot::channel();

        self.tx
            .send(Op::Call {
                method,
                params: RpcParams::new(params),
                send_back: tx,
                policy,
            })
            .map_err(|_| Error::Closed)?;
        rx.await.map_err(|_| Error::Closed)?
    }

    /// Similar to [`Client::request`] but doesn't check
    /// that the params are valid JSON-RPC params.
    pub async fn request_raw(
        &self,
        method: String,
        params: Option<Box<RawValue>>,
    ) -> Result<Box<RawValue>, Error> {
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
    ) -> Result<Subscription, Error> {
        let params = params
            .to_rpc_params()
            .map_err(|e| Error::RpcError(RpcError::ParseError(e)))?;
        self.subscribe_raw_with_policy(subscribe_method, params, unsubscribe_method, policy)
            .await
    }

    /// Create a subscription.
    pub async fn subscribe<P: ToRpcParams>(
        &self,
        subscribe_method: String,
        params: P,
        unsubscribe_method: String,
    ) -> Result<Subscription, Error> {
        let params = params
            .to_rpc_params()
            .map_err(|e| Error::RpcError(RpcError::ParseError(e)))?;
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
    ) -> Result<Subscription, Error> {
        self.subscribe_raw_with_policy(
            subscribe_method,
            params,
            unsubscribe_method,
            CallRetryPolicy::RetryAndResubscribe,
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
    ) -> Result<Subscription, Error> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(Op::Subscription {
                subscribe_method,
                params: RpcParams::new(params),
                unsubscribe_method,
                send_back: tx,
                policy,
            })
            .map_err(|_| Error::Closed)?;
        rx.await.map_err(|_| Error::Closed)?
    }

    /// A future that returns once the client connetion was closed and
    /// it started to reconnect.
    ///
    /// This may be called multiple times.
    pub async fn reconnect_started(&self) {
        self.reconnect.reconnect_started().await
    }

    /// A future that returns once the client connection has been re-established.
    ///
    /// This may be called multiple times.
    pub async fn reconnected(&self) {
        self.reconnect.reconnected().await
    }
    /// Get how many times the client has reconnected successfully.
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
        send_back: oneshot::Sender<Result<Subscription, Error>>,
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
                                tracing::debug!(target: LOG_TARGET, "Failed to reconnect: {e}; terminating the connection");
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
                        tracing::debug!(target: LOG_TARGET, "Failed to reconnect: {e}; terminating the connection");
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
                Err(RpcError::RestartNeeded(_e)) => {
                    if matches!(policy, CallRetryPolicy::Drop) {
                        let _ = send_back.send(Err(Error::DisconnectedWillReconnect));
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
                    let _ = send_back.send(Err(Error::RpcError(e)));
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
                Err(RpcError::RestartNeeded(_e)) => {
                    if matches!(policy, CallRetryPolicy::Drop) {
                        let _ = send_back.send(Err(Error::DisconnectedWillReconnect));
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
                    let _ = send_back.send(Err(Error::RpcError(e)));
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

                    let drop = if matches!(policy, CallRetryPolicy::RetryAndResubscribe) {
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
            // This channel indices whether the main task has been closed.
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

    tracing::debug!(target: LOG_TARGET, "Connection to {url} was closed: `{}`; starting to reconnect", display_close_reason(&close_reason));
    reconnect.reconnect_initiated();

    let client = Retry::new(retry_policy.clone(), || {
        platform::ws_client(url, client_builder)
    })
    .await?;

    reconnect.reconnected();
    tracing::debug!(target: LOG_TARGET, "Connection to {url} was successfully re-established");

    for (id, op) in dispatch {
        pending_calls.push(dispatch_call(client.clone(), op, id, sub_tx.clone()).boxed());
    }

    for (id, s) in open_subscriptions.iter() {
        if !matches!(s.policy, CallRetryPolicy::RetryAndResubscribe) {
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
    use super::*;
    use futures::{
        future::{self, Either},
        stream::FuturesUnordered,
        TryStreamExt,
    };
    use jsonrpsee_server::{
        http, stop_channel, ws, ConnectionGuard, ConnectionState, HttpRequest, HttpResponse,
        RpcModule, RpcServiceBuilder, ServerConfig, SubscriptionMessage,
    };
    use tower::BoxError;
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
        client.reconnect_started().await;

        // Restart the server.
        let (_handle, _) = run_server_with_settings(Some(&addr), false).await.unwrap();

        client.reconnected().await;

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
        client.reconnect_started().await;

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
        client.reconnect_started().await;

        assert!(matches!(sub.next().await, Some(Ok(_))));
        assert!(matches!(sub.next().await, Some(Err(Disconnect::Retry(_)))));

        // Restart the server.
        let (_handle, _) = run_server_with_settings(Some(&addr), false).await.unwrap();

        client.reconnected().await;
        assert_eq!(client.reconnect_count(), 1);

        // Ensure that the client reconnects and that subscription keep running when
        // the connection is established again.
        assert!(sub.next().await.is_some());
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
        client.reconnect_started().await;

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
        client.reconnect_started().await;

        // Restart the server and allow the call to complete.
        let (_handle, _) = run_server_with_settings(Some(&addr), false).await.unwrap();

        assert!(matches!(
            req_fut.await,
            Err(Error::DisconnectedWillReconnect)
        ));
    }

    #[tokio::test]
    async fn reconn_once_when_offline() {
        init_logger();
        let (handle, addr) = run_server_with_settings(None, true).await.unwrap();
        let client = Arc::new(Client::builder().build(addr.clone()).await.unwrap());

        let _ = handle.send(());
        client.reconnect_started().await;

        let client2 = client.clone();
        let reqs = tokio::spawn(async move {
            let futs: FuturesUnordered<_> = (0..10)
                .map(|_| client2.request("say_hello".to_string(), rpc_params![]))
                .collect();

            futs.try_for_each(|_| future::ready(Ok(())))
                .await
                .expect("Requests should be successful");
        });

        // Restart the server and allow the call to complete.
        let (_handle, _) = run_server_with_settings(Some(&addr), false).await.unwrap();

        reqs.await.unwrap();

        assert_eq!(client.reconnect_count(), 1);
    }

    #[tokio::test]
    async fn gives_up_after_ten_retries() {
        init_logger();

        let (handle, addr) = run_server_with_settings(None, true).await.unwrap();
        let client = Client::builder()
            .retry_policy(FixedInterval::from_millis(10).take(10))
            .build(addr.clone())
            .await
            .unwrap();

        let _ = handle.send(());
        client.reconnect_started().await;

        assert!(matches!(
            client.request("say_hello".to_string(), rpc_params![]).await,
            Err(Error::Closed)
        ));
    }

    async fn run_server() -> anyhow::Result<(tokio::sync::broadcast::Sender<()>, String)> {
        run_server_with_settings(None, false).await
    }

    async fn run_server_with_settings(
        url: Option<&str>,
        dont_respond_to_method_calls: bool,
    ) -> anyhow::Result<(tokio::sync::broadcast::Sender<()>, String)> {
        use jsonrpsee_server::HttpRequest;

        let sockaddr = match url {
            Some(url) => url.strip_prefix("ws://").unwrap(),
            None => "127.0.0.1:0",
        };

        let mut i = 0;

        let listener = loop {
            if let Ok(l) = tokio::net::TcpListener::bind(sockaddr).await {
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
            module.register_async_method("say_hello", |_, _, _| async {
                futures::future::pending::<()>().await;
                "timeout"
            })?;
        } else {
            module.register_async_method("say_hello", |_, _, _| async { "lo" })?;
        }

        module.register_subscription(
            "subscribe_lo",
            "subscribe_lo",
            "unsubscribe_lo",
            |_params, pending, _ctx, _| async move {
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

        let (tx, mut rx) = tokio::sync::broadcast::channel(4);
        let tx2 = tx.clone();
        let (stop_handle, server_handle) = stop_channel();
        let addr = listener.local_addr().expect("Could not find local addr");

        tokio::spawn(async move {
            loop {
                let sock = tokio::select! {
                    res = listener.accept() => {
                        match res {
                            Ok((stream, _remote_addr)) => stream,
                            Err(e) => {
                                tracing::error!("Failed to accept connection: {:?}", e);
                                continue;
                            }
                        }
                    }
                    _ = rx.recv() => {
                        break
                    }
                };

                let module = module.clone();
                let rx2 = tx2.subscribe();
                let tx2 = tx2.clone();
                let stop_handle2 = stop_handle.clone();

                let svc = tower::service_fn(move |req: HttpRequest<hyper::body::Incoming>| {
                    let module = module.clone();
                    let tx = tx2.clone();
                    let stop_handle = stop_handle2.clone();

                    let conn_permit = ConnectionGuard::new(1).try_acquire().unwrap();

                    if ws::is_upgrade_request(&req) {
                        let rpc_service = RpcServiceBuilder::new();
                        let conn = ConnectionState::new(stop_handle, 1, conn_permit);

                        async move {
                            let mut rx = tx.subscribe();

                            let (rp, conn_fut) = ws::connect(
                                req,
                                ServerConfig::default(),
                                module,
                                conn,
                                rpc_service,
                            )
                            .await
                            .unwrap();

                            tokio::spawn(async move {
                                tokio::select! {
                                    _ = conn_fut => (),
                                    _ = rx.recv() => {},
                                }
                            });

                            Ok::<_, BoxError>(rp)
                        }
                        .boxed()
                    } else {
                        async { Ok(http::response::denied()) }.boxed()
                    }
                });

                tokio::spawn(serve_with_graceful_shutdown(sock, svc, rx2));
            }

            drop(server_handle);
        });

        Ok((tx, format!("ws://{}", addr)))
    }

    async fn serve_with_graceful_shutdown<S, B, I>(
        io: I,
        service: S,
        mut rx: tokio::sync::broadcast::Receiver<()>,
    ) where
        S: tower::Service<HttpRequest<hyper::body::Incoming>, Response = HttpResponse<B>>
            + Clone
            + Send
            + 'static,
        S::Future: Send,
        S::Response: Send,
        S::Error: Into<BoxError>,
        B: http_body::Body<Data = hyper::body::Bytes> + Send + 'static,
        B::Error: Into<BoxError>,
        I: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + Unpin + 'static,
    {
        if let Err(e) =
            jsonrpsee_server::serve_with_graceful_shutdown(io, service, rx.recv().map(|_| ())).await
        {
            tracing::error!("Error while serving: {:?}", e);
        }
    }
}
