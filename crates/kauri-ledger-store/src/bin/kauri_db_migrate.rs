use kauri_ledger_store::{create_pool, run_migrations};
use std::error::Error;

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let database_url = std::env::var("LEDGER_ADMIN_DATABASE_URL")?;

    let pool = create_pool(&database_url).await?;

    run_migrations(&pool).await?;

    println!("Kauri Ledger database migrations applied successfully");

    pool.close().await;

    Ok(())
}
