use sqlx::{PgPool, postgres::PgPoolOptions};

use crate::error::StoreError;

/// Create a new PostgreSQL connection pool.
pub async fn create_pool(database_url: &str) -> Result<PgPool, StoreError> {
    let pool = PgPoolOptions::new()
        .max_connections(20)
        .connect(database_url)
        .await?;
    Ok(pool)
}

/// Runs all pending migrations
pub async fn run_migrations(pool: &PgPool) -> Result<(), StoreError> {
    sqlx::migrate!("./migrations")
        .run(pool)
        .await
        .map_err(StoreError::MigrationError)?;
    Ok(())
}
