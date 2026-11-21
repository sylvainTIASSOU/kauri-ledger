use kauri_ledger_api::app::App;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    App::run().await
}
