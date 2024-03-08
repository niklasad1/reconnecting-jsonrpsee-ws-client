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

    tracing::info!("Hello from wasm");

    let client = Client::builder()
        .build("wss://rpc.ibp.network/polkadot:443".to_string())
        .await
        .unwrap();

    let rp = client
        .request("chain_getBlockHash".to_string(), rpc_params![19813270])
        .await
        .unwrap();

    assert_eq!("\"0x2990792596bea3bd5e65a868e9510f890cd66cf0002023eb6621b9c0afe930cb\"", rp.get());
}
