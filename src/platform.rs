use crate::{ClientBuilder, RpcError};
use jsonrpsee::core::client::Client;
use std::sync::Arc;

#[cfg(all(feature = "native", not(feature = "web")))]
pub use tokio::spawn;

#[cfg(all(feature = "web", target_arch = "wasm32", not(feature = "native")))]
pub use wasm_bindgen_futures::spawn_local as spawn;

#[cfg(all(feature = "native", not(feature = "web")))]
pub async fn ws_client(url: &str, builder: &ClientBuilder) -> Result<Arc<Client>, RpcError> {
    use jsonrpsee::ws_client::WsClientBuilder;

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

#[cfg(all(feature = "web", target_arch = "wasm32", not(feature = "native")))]
pub async fn ws_client(url: &str, builder: &ClientBuilder) -> Result<Arc<Client>, RpcError> {
    use jsonrpsee::client_transport::web;
    use jsonrpsee::core::client::ClientBuilder as RpseeClientBuilder;

    let ClientBuilder {
        id_kind,
        max_concurrent_requests,
        max_log_len,
        request_timeout,
        ..
    } = builder;

    let (tx, rx) = web::connect(url)
        .await
        .map_err(|e| RpcError::Transport(e.into()))?;

    let ws_client_builder = RpseeClientBuilder::new()
        .max_buffer_capacity_per_subscription(tokio::sync::Semaphore::MAX_PERMITS)
        .max_concurrent_requests(*max_concurrent_requests as usize)
        .set_max_logging_length(*max_log_len)
        .set_tcp_no_delay(true)
        .request_timeout(*request_timeout)
        .id_format(*id_kind);

    let client = ws_client_builder.build_with_wasm(tx, rx);

    Ok(Arc::new(client))
}
