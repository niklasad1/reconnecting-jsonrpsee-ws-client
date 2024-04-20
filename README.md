# reconnecting-jsonrpsee-ws-client

Wrapper crate over the jsonrpsee ws client, which automatically reconnects
under the hood; without that, one has to restart it.
It supports a few retry strategies, such as exponential backoff, but it's also possible
to use custom strategies as long as it implements `Iterator<Item = Duration>`.


By default, the library is re-transmitting pending calls and re-establishing subscriptions that
were closed when the connection was terminated, but it's also possible to disable that
and manage it yourself.


For instance, you may not want to re-subscribe to a subscription
that has side effects or retries at all. Then the library exposes
`request_with_policy` and `subscribe_with_policy` to support that


```text
    let mut sub = client
        .subscribe_with_policy(
            "subscribe_lo".to_string(),
            rpc_params![],
            "unsubscribe_lo".to_string(),
            // Do not re-subscribe if the connection is closed.
            CallRetryPolicy::Retry,
        )
        .await
        .unwrap();
```


The tricky part is subscriptions, which may lose a few notifications
when it's re-connecting, it's not possible to know which ones.


Lost subscription notifications may be very important to know in some cases,
and then this library is not recommended to use.


There is one way to determine how long a reconnection takes:

```text
    // Print when the RPC client starts to reconnect.
    loop {
        rpc.reconnect_started().await;
        let now = std::time::Instant::now();
        rpc.reconnected().await;
        println!(
            "RPC client reconnection took `{} seconds`",
            now.elapsed().as_secs()
        );
    }
```

## Example

```rust
use reconnecting_jsonrpsee_ws_client::{rpc_params, Client, ExponentialBackoff, PingConfig};
use std::time::Duration;

async fn run() {
    // Create a new client with with a reconnecting RPC client.
    let client = Client::builder()
        // Reconnect with exponential backoff and if fails more then 
        // 10 retries we give up and terminate.
        .retry_policy(ExponentialBackoff::from_millis(100).take(10))
        // Send period WebSocket pings/pongs every 6th second and
        // if ACK:ed in 30 seconds then disconnect.
        //
        // This is just a way to ensure that the connection isn't
        // idle if no message is sent that often
        //
        // This only works for native.
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