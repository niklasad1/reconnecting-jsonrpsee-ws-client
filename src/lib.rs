//! Wrapper crate over the jsonrpsee ws client
//! which automatically reconnects under the hood
//! without that the user has to restart it manually
//! by re-transmitting pending calls and re-establish subscriptions
//! that are closed on disconnect.
//!
//! The retry strategy applies globally for connections, method calls
//! and subscriptions.
//!
//! The tricky part is subscription which may lose a few notifications
//! when re-connecting where it's not possible to know which ones.
//!
//! Lost subscription notifications may be very important to know in some scenarios where
//! then crate is not recommended.
//!
//! # Examples
//!
//! ```no run
//! use reconnecting_jsonrpsee_ws_client::{ReconnectingWsClient, ExponentialBackoff};
//!
//! // Connect to a remote node and reconnect with exponential backoff.
//!
//! let client = Client::builder().build(addr).await.unwrap();
//! let mut sub = client.subscribe("subscribe_lo".to_string(), rpc_params![], "unsubscribe_lo".to_string()).await.unwrap();
//! let msg = sub.next().await.unwrap();
//!
//! ```

#![warn(missing_docs)]

mod utils;

pub use jsonrpsee::{
    core::client::error::Error as RpcError, types::SubscriptionId, ws_client::PingConfig,
};
pub use tokio_retry::strategy::*;

const LOG_TARGET: &str = "reconnecting_jsonrpsee_ws_client";

use futures::{future::BoxFuture, FutureExt, Stream, StreamExt};
use jsonrpsee::{
    core::client::{ClientT, Subscription as RpcSubscription, SubscriptionClientT},
    core::{
        client::{IdKind, SubscriptionKind},
        traits::ToRpcParams,
    },
    ws_client::{HeaderMap, WsClient, WsClientBuilder},
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
use tokio_retry::Retry;
use utils::{reconnect_channel, MaybePendingFutures, ReconnectRx, ReconnectTx};

use crate::utils::display_close_reason;

type MethodResult = Result<Box<RawValue>, RpcError>;
type SubscriptionResult = Result<Box<RawValue>, DisconnectWillReconnect>;

/// An opaque error that indicates the subscription
/// was disconnnected and will automatically reconnect.
#[derive(Debug, Clone, thiserror::Error)]
#[error("The client was disconnected and reconnect initiated.")]
pub struct DisconnectWillReconnect;

/// Serialized JSON-RPC params.
#[derive(Debug, Clone)]
pub struct RpcParams(Option<Box<RawValue>>);

impl ToRpcParams for RpcParams {
    fn to_rpc_params(self) -> Result<Option<Box<RawValue>>, serde_json::Error> {
        Ok(self.0)
    }
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
    ping_config: Option<PingConfig>,
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
            ping_config: Some(PingConfig::new()),
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
            ping_config: self.ping_config,
            headers: self.headers,
            max_redirections: self.max_redirections,
            max_log_len: self.max_log_len,
            id_kind: self.id_kind,
            max_concurrent_requests: self.max_concurrent_requests,
            request_timeout: self.request_timeout,
            connection_timeout: self.connection_timeout,
        }
    }

    /// Configure the WebSocket ping/pong interval.
    ///
    /// Default: 30 seconds.
    pub fn enable_ws_ping(mut self, ping_config: PingConfig) -> Self {
        self.ping_config = Some(ping_config);
        self
    }

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
        let client =
            Retry::spawn(self.retry_policy.clone(), || ws_client(url.as_ref(), &self)).await?;
        let (reconn_tx, reconn_rx) = reconnect_channel();

        tokio::spawn(background_task(client, rx, url, reconn_tx, self));

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
        let params = RpcParams(params.to_rpc_params()?);
        self.request_raw(method, params).await
    }

    /// Similar to [`Client::request`] but doesn't check
    /// that the params are valid JSON-RPC params.
    pub async fn request_raw(
        &self,
        method: String,
        params: RpcParams,
    ) -> Result<Box<RawValue>, RpcError> {
        let (tx, rx) = oneshot::channel();

        self.tx
            .send(Op::Call {
                method,
                params,
                send_back: tx,
            })
            .map_err(|_| RpcError::Custom("Client is dropped".to_string()))?;
        let rp = rx
            .await
            .map_err(|_| RpcError::Custom("Client is dropped".to_string()))?;
        rp
    }

    /// Create a subscription.
    pub async fn subscribe<P: ToRpcParams>(
        &self,
        subscribe_method: String,
        params: P,
        unsubscribe_method: String,
    ) -> Result<Subscription, RpcError> {
        let params = RpcParams(params.to_rpc_params()?);
        self.subscribe_raw(subscribe_method, params, unsubscribe_method)
            .await
    }

    /// Similar to [`Client::subscribe`] but doesn't check
    /// that the params are valid JSON-RPC params.
    pub async fn subscribe_raw(
        &self,
        subscribe_method: String,
        params: RpcParams,
        unsubscribe_method: String,
    ) -> Result<Subscription, RpcError> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(Op::Subscription {
                subscribe_method,
                params,
                unsubscribe_method,
                send_back: tx,
            })
            .map_err(|_| RpcError::Custom("Client is dropped".to_string()))?;
        rx.await
            .map_err(|_| RpcError::Custom("Client is dropped".to_string()))?
    }

    /// A future that returns once the client reconnects.
    ///
    /// This may be called multiple times.
    pub async fn on_reconnect(&self) {
        self.reconnect.on_reconnect().await
    }

    /// Get how many times the client has reconnected.
    ///
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

#[cfg(feature = "subxt")]
impl subxt::backend::rpc::RpcClientT for Client {
    fn request_raw<'a>(
        &'a self,
        method: &'a str,
        params: Option<Box<RawValue>>,
    ) -> subxt::backend::rpc::RawRpcFuture<'a, Box<serde_json::value::RawValue>> {
        async {
            self.request_raw(method.to_string(), RpcParams(params))
                .await
                .map_err(|e| subxt::error::RpcError::ClientError(Box::new(e)))
        }
        .boxed()
    }

    fn subscribe_raw<'a>(
        &'a self,
        sub: &'a str,
        params: Option<Box<RawValue>>,
        unsub: &'a str,
    ) -> subxt::backend::rpc::RawRpcFuture<'a, subxt::backend::rpc::RawRpcSubscription> {
        use futures::TryStreamExt;

        async {
            let sub = self
                .subscribe_raw(sub.to_string(), RpcParams(params), unsub.to_string())
                .await
                .map_err(|e| subxt::error::RpcError::ClientError(Box::new(e)))?;

            let id = match sub.id {
                SubscriptionId::Num(n) => n.to_string(),
                SubscriptionId::Str(s) => s.to_string(),
            };
            let stream = tokio_stream::wrappers::UnboundedReceiverStream::new(sub.stream)
                .map_err(|e| subxt::error::RpcError::ClientError(Box::new(e)))
                .boxed();

            Ok(subxt::backend::rpc::RawRpcSubscription {
                stream,
                id: Some(id),
            })
        }
        .boxed()
    }
}

#[derive(Debug)]
enum Op {
    Call {
        method: String,
        params: RpcParams,
        send_back: oneshot::Sender<MethodResult>,
    },
    Subscription {
        subscribe_method: String,
        params: RpcParams,
        unsubscribe_method: String,
        send_back: oneshot::Sender<Result<Subscription, RpcError>>,
    },
}

#[derive(Debug)]
struct RetrySubscription {
    tx: mpsc::UnboundedSender<SubscriptionResult>,
    subscribe_method: String,
    params: RpcParams,
    unsubscribe_method: String,
}

#[derive(Debug)]
struct Closed {
    op: Op,
    id: usize,
}

async fn ws_client<P>(url: &str, builder: &ClientBuilder<P>) -> Result<Arc<WsClient>, RpcError> {
    let ClientBuilder {
        max_request_size,
        max_response_size,
        ping_config,
        headers,
        max_redirections,
        id_kind,
        max_concurrent_requests,
        max_log_len,
        request_timeout,
        connection_timeout,
        ..
    } = builder;

    let mut ws_client_builder = WsClientBuilder::new()
        .max_request_size(*max_request_size)
        .max_response_size(*max_response_size)
        .set_headers(headers.clone())
        .max_redirections(*max_redirections as usize)
        .max_buffer_capacity_per_subscription(tokio::sync::Semaphore::MAX_PERMITS)
        .max_concurrent_requests(*max_concurrent_requests as usize)
        .set_max_logging_length(*max_log_len)
        .set_tcp_no_delay(true)
        .request_timeout(*request_timeout)
        .connection_timeout(*connection_timeout)
        .id_format(*id_kind);

    if let Some(ping) = ping_config {
        ws_client_builder = ws_client_builder.enable_ws_ping(*ping);
    }

    let client = ws_client_builder.build(url).await?;

    Ok(Arc::new(client))
}

async fn dispatch_call(
    client: Arc<WsClient>,
    op: Op,
    id: usize,
    sub_closed: mpsc::UnboundedSender<usize>,
) -> Result<Option<(usize, RetrySubscription)>, Closed> {
    match op {
        Op::Call {
            method,
            params,
            send_back,
        } => {
            match client
                .request::<Box<RawValue>, _>(&method, params.clone())
                .await
            {
                Ok(rp) => {
                    // Fails only if the request is dropped.
                    let _ = send_back.send(Ok(rp));
                    Ok(None)
                }
                Err(RpcError::RestartNeeded(_)) => Err(Closed {
                    op: Op::Call {
                        method,
                        params,
                        send_back,
                    },
                    id,
                }),
                Err(e) => {
                    // Fails only if the request is dropped.
                    let _ = send_back.send(Err(e));
                    Ok(None)
                }
            }
        }
        Op::Subscription {
            subscribe_method,
            params,
            unsubscribe_method,
            send_back,
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

                    tokio::spawn(subscription_handler(tx.clone(), sub, sub_closed, id));

                    let sub = RetrySubscription {
                        tx,
                        subscribe_method,
                        params,
                        unsubscribe_method,
                    };

                    let stream = Subscription {
                        id: sub_id,
                        stream: rx,
                    };

                    // Fails only if the request is dropped.
                    let _ = send_back.send(Ok(stream));
                    Ok(Some((id, sub)))
                }
                Err(RpcError::RestartNeeded(_)) => Err(Closed {
                    op: Op::Subscription {
                        subscribe_method,
                        params,
                        unsubscribe_method,
                        send_back,
                    },
                    id,
                }),
                Err(e) => {
                    // Fails only if the request is dropped.
                    let _ = send_back.send(Err(e));
                    Ok(None)
                }
            }
        }
    }
}

/// Sends a message to main task if the subscription was closed without that connection was closed.
async fn subscription_handler(
    tx: UnboundedSender<SubscriptionResult>,
    mut sub: RpcSubscription<Box<RawValue>>,
    sub_closed: mpsc::UnboundedSender<usize>,
    id: usize,
) {
    let sub_dropped = loop {
        tokio::select! {
            next_msg = sub.next() => {
                let Some(notif) = next_msg else {
                    _ = tx.send(Err(DisconnectWillReconnect));
                    break false;
                };

                let msg = notif.expect("RawValue is valid JSON; qed");
                if tx.send(Ok(msg)).is_err() {
                    break true;
                }
            }
            _ = sub_closed.closed() => {
                break true
            }
        }
    };

    // The subscription was dropped.
    // Thus, the subscription should be removed.
    if sub_dropped {
        let _ = sub_closed.send(id);
    }
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

            // Handle response.
            next_response = pending_calls.next() => {
                match next_response {
                    Some(Ok(Some((id, sub)))) => {
                        open_subscriptions.insert(id, sub);
                    }
                    // The connection was closed, re-connect and try to send all messages again.
                    Some(Err(Closed { op, id })) => {
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
                    _ => (),
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

struct ReconnectParams<'a, P> {
    url: &'a str,
    pending_calls: &'a mut MaybePendingFutures<
        BoxFuture<'static, Result<Option<(usize, RetrySubscription)>, Closed>>,
    >,
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
            Some(Ok(_)) | None => {}
            Some(Err(Closed { op, id })) => {
                dispatch.push((id, op));
            }
        };
    }

    tracing::debug!(target: LOG_TARGET, "Connection was closed: `{}`", display_close_reason(&close_reason));

    reconnect.reconnect();
    let client = Retry::spawn(retry_policy.clone(), || ws_client(url, client_builder)).await?;

    for (id, op) in dispatch {
        pending_calls.push(dispatch_call(client.clone(), op, id, sub_tx.clone()).boxed());
    }

    for (id, s) in open_subscriptions.iter() {
        let sub = Retry::spawn(retry_policy.clone(), || {
            client.subscribe::<Box<RawValue>, _>(
                &s.subscribe_method,
                s.params.clone(),
                &s.unsubscribe_method,
            )
        })
        .await?;

        tokio::spawn(subscription_handler(s.tx.clone(), sub, sub_tx.clone(), *id));
    }

    Ok(client)
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::future::Either;
    use jsonrpsee::{
        rpc_params,
        server::{Server, ServerHandle},
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

        let _ = handle.stop();
        handle.stopped().await;

        // Restart the server.
        let (_handle, _) = run_server_with_settings(Some(&addr), false).await.unwrap();

        client.on_reconnect().await;

        // Ensure that the client reconnects and that subscription keep running when
        // the connection is established again.
        for _ in 0..10 {
            assert!(sub.next().await.is_some());
        }

        assert_eq!(client.reconnect_count(), 1);
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

        let _ = handle.stop();
        handle.stopped().await;

        client.on_reconnect().await;

        assert!(matches!(sub.next().await, Some(Ok(_))));
        assert!(matches!(
            sub.next().await,
            Some(Err(DisconnectWillReconnect))
        ));

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

        let _ = handle.stop();
        handle.stopped().await;

        // Restart the server and allow the call to complete.
        let (_handle, _) = run_server_with_settings(Some(&addr), false).await.unwrap();

        assert!(req_fut.await.is_ok());
    }

    async fn run_server() -> anyhow::Result<(ServerHandle, String)> {
        run_server_with_settings(None, false).await
    }

    async fn run_server_with_settings(
        url: Option<&str>,
        dont_respond_to_method_calls: bool,
    ) -> anyhow::Result<(ServerHandle, String)> {
        let sockaddr = match url {
            Some(url) => url.strip_prefix("ws://").unwrap(),
            None => "127.0.0.1:0",
        };

        let server = Server::builder().build(sockaddr).await?;
        let mut module = RpcModule::new(());

        if dont_respond_to_method_calls {
            module.register_async_method("say_hello", |_, _| async {
                tokio::time::sleep(Duration::from_secs(10)).await;
                "lo"
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

        let addr = server.local_addr()?;
        let handle = server.start(module);

        Ok((handle, format!("ws://{}", addr)))
    }
}
