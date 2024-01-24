# reconnecting-jsonrpsee-ws-client

Wrapper crate over the jsonrpsee ws client, which automatically reconnects
under the hood; without that, the user has to restart it manually by
re-transmitting pending calls and re-establish subscriptions that
were closed when the connection was terminated.

The tricky part is subscription, which may lose a few notifications,
then re-connect where it's not possible to know which ones.

Lost subscription notifications may be very important to know in some cases,
and then this library is not recommended to use.

## Example

```rust
use reconnecting_jsonrpsee_ws_client::{rpc_params, Client, ExponentialBackoff, PingConfig};
use std::time::Duration;

async fn run() {
    // Create a new client with with a reconnecting RPC client.
    let client = Client::builder()
        // Reconnect with exponential backoff.
        .retry_policy(ExponentialBackoff::from_millis(100))
        // Send period WebSocket pings/pongs every 6th second and
        // if ACK:ed in 30 seconds then disconnect.
        //
        // This is just a way to ensure that the connection isn't
        // idle if no message is sent that often
        .enable_ws_ping(
            PingConfig::new()
                .ping_interval(Duration::from_secs(6))
                .inactive_limit(Duration::from_secs(30)),
        )
        .build("ws://localhost:9944".to_string())
        .await
        .unwrap();

    // make a JSON-RPC call
    let json = client
        .request("say_hello".to_string(), rpc_params![])
        .await
        .unwrap();

    // make JSON-RPC subscription.
    let mut sub = client
        .subscribe(
            "subscribe_lo".to_string(),
            rpc_params![],
            "unsubscribe_lo".to_string(),
        )
        .await
        .unwrap();
    let notif = sub.next().await.unwrap();
}
```
