//! Wrapper crate over the jsonrpsee ws client
//! which automatically reconnects under the hood
//! without that the user has to restart it manually
//! by re-transmitting pending calls and re-establish subscriptions
//! that normally be closed on disconnect.
//!
//! The tricky part is subscription which may loose a few notifications
//! when re-connecting where it's not possible to know which ones.
//!
//! Lost subscription notifications may be very important to know in some scenarios where
//! this crate is not recommended.
//!
//! # Examples
//!
//! ```no run
//! use reconnecting_jsonrpsee_ws_client::{ReconnectingWsClient, ExponentialBackoff};
//!
//! // Connect to a remote node a retry with exponential backoff.
//!
//! let client = ReconnectingWsClient::new("ws://example.com", ExponentialBackoff::from_millis(10), PingConfig::Enabled(Duration::from_secs(6)))).await.unwrap();
//! let mut sub = client.subscribe("subscribe_lo".to_string(), rpc_params![], "unsubscribe_lo".to_string()).await.unwrap();
//! let msg = sub.recv().await.unwrap();
//!
//! ```

pub mod utils;

use futures::{future::BoxFuture, FutureExt, StreamExt};
use jsonrpsee::{
    core::client::{ClientT, Subscription},
    core::Error as RpcError,
    core::{client::SubscriptionClientT, params::ArrayParams},
    ws_client::{WsClient, WsClientBuilder},
};
use std::{collections::HashMap, sync::Arc, time::Duration};
use tokio::sync::{
    mpsc::{self, UnboundedReceiver, UnboundedSender},
    oneshot,
};
pub use tokio_retry::strategy::*;
use tokio_retry::Retry;
use utils::{MaybePendingFutures, ReconnectCounter};

/// JSON-RPC client that reconnects automatically and may loose
/// subscription notifications when it reconnects.
#[derive(Clone)]
pub struct ReconnectingWsClient {
    tx: mpsc::UnboundedSender<Op>,
    reconnect_cnt: ReconnectCounter,
}

/// Websocket ping/pong configuration.
#[derive(Debug, Clone, Copy)]
pub enum PingConfig {
    /// Disabled.
    Disabled,
    /// Pings are sent out specified interval.
    Enabled(Duration),
}

impl ReconnectingWsClient {
    pub async fn request(
        &self,
        method: String,
        params: ArrayParams,
    ) -> Result<serde_json::Value, RpcError> {
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
        tracing::trace!("RPC response: {:?}", rp);
        rp
    }

    pub async fn subscribe(
        &self,
        subscribe_method: String,
        params: ArrayParams,
        unsubscribe_method: String,
    ) -> Result<mpsc::UnboundedReceiver<Result<serde_json::Value, RpcError>>, RpcError> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(Op::Subscription {
                subscribe_method,
                params,
                unsubscribe_method,
                send_back: tx,
            })
            .map_err(|_| RpcError::Custom("Client is dropped".to_string()))?;
        let rp = rx
            .await
            .map_err(|_| RpcError::Custom("Client is dropped".to_string()))?;
        rp
    }

    pub fn retry_count(&self) -> usize {
        self.reconnect_cnt.get()
    }
}

impl ReconnectingWsClient {
    pub async fn new<P>(
        url: String,
        retry_policy: P,
        ping_config: PingConfig,
    ) -> Result<Self, RpcError>
    where
        P: Iterator<Item = Duration> + Send + 'static + Clone,
    {
        let (tx, rx) = mpsc::unbounded_channel();
        let client = Retry::spawn(retry_policy.clone(), || ws_client(&url, ping_config)).await?;
        let reconnect_cnt = ReconnectCounter::new();

        tokio::spawn(background_task(
            client,
            rx,
            retry_policy,
            url,
            reconnect_cnt.clone(),
            ping_config,
        ));

        Ok(Self { tx, reconnect_cnt })
    }
}

#[derive(Debug)]
pub enum Op {
    Call {
        method: String,
        params: ArrayParams,
        send_back: oneshot::Sender<Result<serde_json::Value, RpcError>>,
    },
    Subscription {
        subscribe_method: String,
        params: ArrayParams,
        unsubscribe_method: String,
        send_back: oneshot::Sender<
            Result<mpsc::UnboundedReceiver<Result<serde_json::Value, RpcError>>, RpcError>,
        >,
    },
}

#[derive(Debug)]
struct RetrySubscription {
    tx: mpsc::UnboundedSender<Result<serde_json::Value, RpcError>>,
    subscribe_method: String,
    params: ArrayParams,
    unsubscribe_method: String,
}

#[derive(Debug)]
pub struct Closed {
    op: Op,
    id: usize,
}

async fn ws_client(url: &str, ping_config: PingConfig) -> Result<Arc<WsClient>, RpcError> {
    let client = if let PingConfig::Enabled(dur) = ping_config {
        WsClientBuilder::default()
            .ping_interval(dur)
            .build(url)
            .await?
    } else {
        WsClientBuilder::default().build(url).await?
    };

    Ok(Arc::new(client))
}

async fn dispatch_call(
    client: Arc<WsClient>,
    op: Op,
    id: usize,
    reconnect_cnt: ReconnectCounter,
    sub_closed: mpsc::UnboundedSender<usize>,
) -> Result<Option<(usize, RetrySubscription)>, Closed> {
    match op {
        Op::Call {
            method,
            params,
            send_back,
        } => {
            match client
                .request::<serde_json::Value, _>(&method, params.clone())
                .await
            {
                Ok(rp) => {
                    send_back.send(Ok(rp)).unwrap();
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
                    send_back.send(Err(e)).unwrap();
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
                .subscribe::<serde_json::Value, _>(
                    &subscribe_method,
                    params.clone(),
                    &unsubscribe_method,
                )
                .await
            {
                Ok(sub) => {
                    let (tx, rx) = mpsc::unbounded_channel();

                    tokio::spawn(subscription_handler(
                        tx.clone(),
                        sub,
                        reconnect_cnt,
                        sub_closed,
                        id,
                    ));

                    let sub = RetrySubscription {
                        tx,
                        subscribe_method,
                        params,
                        unsubscribe_method,
                    };

                    let _ = send_back.send(Ok(rx));
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
                    let _ = send_back.send(Err(e));
                    Ok(None)
                }
            }
        }
    }
}

/// Sends a message to main task if subscription was closed without that connection was closed.
async fn subscription_handler(
    tx: UnboundedSender<Result<serde_json::Value, RpcError>>,
    mut sub: Subscription<serde_json::Value>,
    reconnect_cnt: ReconnectCounter,
    sub_closed: mpsc::UnboundedSender<usize>,
    id: usize,
) {
    let restart_cnt_before = reconnect_cnt.get();

    let sub_dropped = loop {
        tokio::select! {
            next_msg = sub.next() => {
                let Some(notif) = next_msg else {
                    break false;
                };

                if tx.send(notif).is_err() {
                    break true;
                }
            }
            _ = sub_closed.closed() => break true,
        }
    };

    // The subscription was dropped by the user or closed by some reason.
    //
    // Thus, the subscription should be removed.
    if sub_dropped || restart_cnt_before != reconnect_cnt.get() {
        let _ = sub_closed.send(id);
    }
}

async fn background_task<P>(
    mut client: Arc<WsClient>,
    mut rx: UnboundedReceiver<Op>,
    retry_policy: P,
    url: String,
    reconnect_cnt: ReconnectCounter,
    ping_config: PingConfig,
) where
    P: Iterator<Item = Duration> + Send + 'static + Clone,
{
    let (sub_tx, mut sub_rx) = mpsc::unbounded_channel();
    let mut pending_calls = MaybePendingFutures::new();
    let mut open_subscriptions = HashMap::new();
    let mut id = 0;

    loop {
        tracing::trace!(
            "pending_calls={} open_subscriptions={}, client_restarts={}",
            pending_calls.len(),
            open_subscriptions.len(),
            reconnect_cnt.get(),
        );

        tokio::select! {

            // An incoming JSON-RPC call to dispatch.
            next_message = rx.recv() => {
                tracing::trace!("next_message: {:?}", next_message);

                match next_message {
                    None => break,
                    Some(op) => {
                        pending_calls.push(dispatch_call(client.clone(), op, id, reconnect_cnt.clone(), sub_tx.clone()).boxed());
                    }
                };
            }

            // Handle response.
            next_response = pending_calls.next() => {
                tracing::trace!("next_response: {:?}", next_response);

                match next_response {
                    Some(Ok(Some((id, sub)))) => {
                        open_subscriptions.insert(id, sub);
                    }
                    // The connection was closed, re-connect and try to send all messages again.
                    Some(Err(Closed { op, id })) => {
                        client = match reconnect(&url, ping_config, &mut pending_calls, vec![(id, op)], reconnect_cnt.clone(), retry_policy.clone(), sub_tx.clone(), &open_subscriptions).await {
                            Ok(client) => client,
                            Err(e) => {
                                tracing::error!("Failed to reconnect/re-establish subscriptions: {e}; terminating the connection");
                                break;
                            }
                       };
                    }
                    _ => (),
                }
            }

            // The connection was terminated and try to reconnect.
            _ = client.on_disconnect() => {
               client = match reconnect(&url, ping_config, &mut pending_calls, Vec::new(), reconnect_cnt.clone(), retry_policy.clone(), sub_tx.clone(), &open_subscriptions).await {
                    Ok(client) => client,
                    Err(e) => {
                        tracing::error!("Failed to reconnect/re-establish subscriptions: {e}; terminating the connection");
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

async fn reconnect<P>(
    url: &str,
    ping_config: PingConfig,
    pending_calls: &mut MaybePendingFutures<
        BoxFuture<'static, Result<Option<(usize, RetrySubscription)>, Closed>>,
    >,
    mut dispatch: Vec<(usize, Op)>,
    reconnect_cnt: ReconnectCounter,
    retry_policy: P,
    sub_tx: UnboundedSender<usize>,
    open_subscriptions: &HashMap<usize, RetrySubscription>,
) -> Result<Arc<WsClient>, RpcError>
where
    P: Iterator<Item = Duration> + Send + 'static + Clone,
{
    tracing::info!("Connection closed; reconnecting");

    // All futures should return now because the connection has been terminated.
    while !pending_calls.is_empty() {
        match pending_calls.next().await {
            Some(Ok(_)) | None => {}
            Some(Err(Closed { op, id })) => {
                dispatch.push((id, op));
            }
        };
    }

    reconnect_cnt.inc();
    let client = Retry::spawn(retry_policy.clone(), || ws_client(url, ping_config)).await?;

    for (id, op) in dispatch {
        pending_calls.push(
            dispatch_call(
                client.clone(),
                op,
                id,
                reconnect_cnt.clone(),
                sub_tx.clone(),
            )
            .boxed(),
        );
    }

    for (id, s) in open_subscriptions.iter() {
        let sub = Retry::spawn(retry_policy.clone(), || {
            client.subscribe::<serde_json::Value, _>(
                &s.subscribe_method,
                s.params.clone(),
                &s.unsubscribe_method,
            )
        })
        .await?;

        tokio::spawn(subscription_handler(
            s.tx.clone(),
            sub,
            reconnect_cnt.clone(),
            sub_tx.clone(),
            *id,
        ));
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
        let filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap();
        _ = tracing_subscriber::fmt()
            .with_env_filter(filter)
            .finish()
            .try_init();
    }

    #[tokio::test]
    async fn call_works() {
        init_logger();
        let (_handle, addr) = run_server().await.unwrap();

        let client = ReconnectingWsClient::new(
            addr,
            ExponentialBackoff::from_millis(10),
            PingConfig::Disabled,
        )
        .await
        .unwrap();

        assert!(client
            .request("say_hello".to_string(), rpc_params![])
            .await
            .is_ok(),)
    }

    #[tokio::test]
    async fn sub_works() {
        init_logger();
        let (_handle, addr) = run_server().await.unwrap();

        let client = ReconnectingWsClient::new(
            addr,
            ExponentialBackoff::from_millis(10),
            PingConfig::Disabled,
        )
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

        assert!(sub.recv().await.is_some());
    }

    #[tokio::test]
    async fn reconn_sub_works() {
        init_logger();
        let (handle, addr) = run_server().await.unwrap();

        let client = ReconnectingWsClient::new(
            addr.clone(),
            ExponentialBackoff::from_millis(10),
            PingConfig::Disabled,
        )
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

        let _ = handle.stop();
        handle.stopped().await;

        // Restart the server.
        let (_handle, _) = run_server_with_settings(Some(&addr), false).await.unwrap();

        // Ensure that the client reconnects and that subscription keep running when
        // the connection is established again.
        for _ in 0..10 {
            assert!(sub.recv().await.is_some());
        }

        assert_eq!(client.retry_count(), 1);
    }

    #[tokio::test]
    async fn reconn_calls_works() {
        init_logger();
        let (handle, addr) = run_server_with_settings(None, true).await.unwrap();

        let client = Arc::new(
            ReconnectingWsClient::new(
                addr.clone(),
                ExponentialBackoff::from_millis(10),
                PingConfig::Disabled,
            )
            .await
            .unwrap(),
        );

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
