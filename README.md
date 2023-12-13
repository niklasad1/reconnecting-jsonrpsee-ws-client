# reconnecting-jsonrpsee-ws-client

Wrapper crate over the jsonrpsee ws client which automatically reconnects under the hood
without that the user has to restart it manually by re-transmitting pending calls 
and re-establish subscriptions that where closed when the connection was terminated.

This is crate is temporary fix for subxt because it's not easy to implement
reconnect-logic on-top of it the moment.

The tricky part is subscription which may loose a few notifications then re-connecting 
where it's not possible to know which ones.
Lost subscription notifications may be very important to know in some scenarios then
this crate is not recommended.

## Example

```rust
let client = Client::builder().build(addr).await.unwrap();
let mut sub = client
    .subscribe("sub".to_string(), rpc_params![], "unsub".to_string()).await.unwrap();
let msg = sub.recv().await.unwrap();
```
