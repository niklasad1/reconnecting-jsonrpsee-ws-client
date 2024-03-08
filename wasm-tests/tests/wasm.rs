#![cfg(target_arch = "wasm32")]

use reconnecting_jsonrpsee_ws_client::{rpc_params, Client};
use wasm_bindgen_test::*;

wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

/// Run the tests by `$ wasm-pack test --firefox --headless`

fn init_tracing() {
    console_error_panic_hook::set_once();
    tracing_wasm::set_as_global_default();
}

#[wasm_bindgen_test]
async fn rpc_method_call_works() {
    init_tracing();

    let client = Client::builder()
        .build("wss://echo.websocket.org/.ws".to_string())
        .await
        .unwrap();

    let rp = client
        .request("echo".to_string(), rpc_params![])
        .await
        .unwrap();

    assert_eq!("echo", rp.get());
}
