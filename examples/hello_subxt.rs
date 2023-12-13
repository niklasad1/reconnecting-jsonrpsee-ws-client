use reconnecting_jsonrpsee_ws_client::Client;
use subxt::backend::rpc::RpcClient;
use subxt::{OnlineClient, PolkadotConfig};
use subxt_signer::sr25519::dev;
use tracing_subscriber::util::SubscriberInitExt;

// Generate an interface that we can use from the node's metadata.
#[subxt::subxt(runtime_metadata_path = "examples/metadata.scale")]
pub mod runtime {}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let filter = tracing_subscriber::EnvFilter::from_default_env();
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .finish()
        .init();

    let rpc = Client::builder()
        .build("ws://localhost:9944".to_string())
        .await?;

    // Create a new API client, configured to talk to Polkadot nodes.
    let api: OnlineClient<PolkadotConfig> =
        OnlineClient::from_rpc_client(RpcClient::new(rpc)).await?;

    // Build a balance transfer extrinsic.
    let dest = dev::bob().public_key().into();
    let balance_transfer_tx = runtime::tx().balances().transfer_allow_death(dest, 10_000);

    // Submit the balance transfer extrinsic from Alice, and wait for it to be successful
    // and in a finalized block. We get back the extrinsic events if all is well.
    let from = dev::alice();
    let events = api
        .tx()
        .sign_and_submit_then_watch_default(&balance_transfer_tx, &from)
        .await?
        .wait_for_finalized_success()
        .await?;

    // Find a Transfer event and print it.
    let transfer_event = events.find_first::<runtime::balances::events::Transfer>()?;
    if let Some(event) = transfer_event {
        println!("Balance transfer success: {event:?}");
    }

    Ok(())
}
