pub mod utils;

use futures::StreamExt;
use jsonrpsee::{
    core::client::{ClientT, Subscription},
    core::Error as RpcError,
    core::{client::SubscriptionClientT, params::ArrayParams},
    ws_client::{WsClient, WsClientBuilder},
};
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::sync::{
    mpsc::{self, UnboundedReceiver, UnboundedSender},
    oneshot,
};
pub use tokio_retry::strategy::*;
use tokio_retry::Retry;
use utils::MaybePendingFutures;

/// JSON-RPC client that reconnects automatically and may loose
/// subscription notifications when it reconnects.
#[derive(Clone)]
pub struct ReconnectingWsClient {
    tx: mpsc::UnboundedSender<Op>,
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
        tracing::trace!("call response: {:?}", rp);
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
        tracing::trace!("sub: {:?}", rp);
        rp
    }
}

impl ReconnectingWsClient {
    pub async fn new<P>(url: String, retry_policy: P) -> Result<Self, RpcError>
    where
        P: Iterator<Item = Duration> + Send + 'static + Clone,
    {
        let (tx, rx) = mpsc::unbounded_channel();
        let client = Retry::spawn(retry_policy.clone(), || ws_client(&url)).await?;

        tokio::spawn(background_task(client, rx, retry_policy, url));

        Ok(Self { tx })
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

async fn ws_client(url: &str) -> Result<Arc<WsClient>, RpcError> {
    let client = WsClientBuilder::default().build(url).await?;
    Ok(Arc::new(client))
}

async fn dispatch_call(
    client: Arc<WsClient>,
    op: Op,
    id: usize,
    client_restart: Arc<AtomicUsize>,
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
                        client_restart,
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
                    id: id,
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
    client_restart: Arc<AtomicUsize>,
    sub_closed: mpsc::UnboundedSender<usize>,
    id: usize,
) {
    let restart_cnt_before = client_restart.load(std::sync::atomic::Ordering::SeqCst);

    while let Some(res) = sub.next().await {
        let _ = tx.send(res);
    }

    let restart_cnt_after = client_restart.load(std::sync::atomic::Ordering::SeqCst);

    if restart_cnt_before != restart_cnt_after {
        let _ = sub_closed.send(id);
    }
}

async fn background_task<P>(
    mut client: Arc<WsClient>,
    mut rx: UnboundedReceiver<Op>,
    retry_policy: P,
    url: String,
) where
    P: Iterator<Item = Duration> + Send + 'static + Clone,
{
    let (sub_tx, mut sub_rx) = mpsc::unbounded_channel();
    let mut pending_calls = MaybePendingFutures::new();
    let mut open_subscriptions = HashMap::new();
    let mut id = 0;
    let client_restart = Arc::new(AtomicUsize::new(0));

    loop {
        tokio::select! {
            // An incoming JSON-RPC call to dispatch.
            next_message = rx.recv() => {
                tracing::trace!("next_message: {:?}", next_message);

                match next_message {
                    None => break,
                    Some(op) => {
                        pending_calls.push(dispatch_call(client.clone(), op, id, client_restart.clone(), sub_tx.clone()));
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
                        tracing::info!("Connection closed; time to reconnect");

                        let mut dispatch = vec![(id, op)];

                        // All futures should return now because the connection has been terminated.
                        while !pending_calls.is_empty() {
                            match pending_calls.next().await {
                                Some(Ok(_)) | None => {}
                                Some(Err(Closed { op, id })) => {
                                    dispatch.push((id, op));
                                }
                            };
                        }

                        client_restart.fetch_add(1, Ordering::SeqCst);
                        let client = Retry::spawn(retry_policy.clone(), || ws_client(&url)).await.unwrap();

                        for (id, op) in dispatch {
                            pending_calls.push(dispatch_call(client.clone(), op, id, client_restart.clone(), sub_tx.clone()));
                        }

                        for (id, s) in open_subscriptions.iter() {
                            let sub = Retry::spawn(retry_policy.clone(), || client.subscribe::<serde_json::Value, _>(&s.subscribe_method, s.params.clone(), &s.unsubscribe_method)).await.unwrap();

                            tokio::spawn(subscription_handler(
                                s.tx.clone(),
                                sub,
                                client_restart.clone(),
                                sub_tx.clone(),
                                *id,
                            ));
                        }

                    }
                    _ => (),
                }
            }

            _ = client.on_disconnect() => {
                tracing::info!("Connection closed; reconnecting");

                let mut dispatch = Vec::new();

                // All futures should return now because the connection has been terminated.
                while !pending_calls.is_empty() {
                    match pending_calls.next().await {
                        Some(Ok(_)) | None => {}
                        Some(Err(Closed { op, id })) => {
                            dispatch.push((id, op));
                        }
                    };
                }

                client_restart.fetch_add(1, Ordering::SeqCst);
                client = Retry::spawn(retry_policy.clone(), || ws_client(&url)).await.unwrap();

                for (id, op) in dispatch {
                    pending_calls.push(dispatch_call(client.clone(), op, id, client_restart.clone(), sub_tx.clone()));
                }

                for (id, s) in open_subscriptions.iter() {
                    let sub = Retry::spawn(retry_policy.clone(), || client.subscribe::<serde_json::Value, _>(&s.subscribe_method, s.params.clone(), &s.unsubscribe_method)).await.unwrap();

                    tokio::spawn(subscription_handler(
                        s.tx.clone(),
                        sub,
                        client_restart.clone(),
                        sub_tx.clone(),
                        *id,
                    ));
                }
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

#[cfg(test)]
mod tests {
    use super::*;
    use jsonrpsee::{
        rpc_params,
        server::{Server, ServerHandle},
        RpcModule, SubscriptionMessage,
    };

    #[tokio::test]
    async fn call_works() {
        _ = tracing_subscriber::fmt().try_init();
        let (_handle, addr) = run_server(None).await.unwrap();

        let client = ReconnectingWsClient::new(addr, ExponentialBackoff::from_millis(10))
            .await
            .unwrap();

        assert!(client
            .request("say_hello".to_string(), rpc_params![])
            .await
            .is_ok(),)
    }

    #[tokio::test]
    async fn sub_works() {
        _ = tracing_subscriber::fmt().try_init();
        let (_handle, addr) = run_server(None).await.unwrap();

        let client = ReconnectingWsClient::new(addr, ExponentialBackoff::from_millis(10))
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
    async fn reconn_works() {
        _ = tracing_subscriber::fmt().try_init();
        let (handle, addr) = run_server(None).await.unwrap();

        let client = ReconnectingWsClient::new(addr.clone(), ExponentialBackoff::from_millis(10))
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
        let (_handle, _) = run_server(Some(&addr)).await.unwrap();

        // Ensure that the client reconnects and that subscription keep running when
        // the connection is established again.
        for _ in 0..10 {
            assert!(sub.recv().await.is_some());
        }
    }

    async fn run_server(url: Option<&str>) -> anyhow::Result<(ServerHandle, String)> {
        let sockaddr = match url {
            Some(url) => url.strip_prefix("ws://").unwrap(),
            None => "127.0.0.1:0",
        };

        let server = Server::builder().build(sockaddr).await?;
        let mut module = RpcModule::new(());

        module.register_method("say_hello", |_, _| "lo")?;
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
