use kauri_ledger::app::App;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    App::run().await
}