use kauri_ledger_store::{create_pool, run_migrations};
use sqlx::PgPool;
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;

/// A test fixture that starts a real PostgreSQL container, runs migrations,
/// and provides a connection pool.
pub struct TestDb {
    pub pool: PgPool,
    // Keep the container alive for the duration of the test
    _container: testcontainers::ContainerAsync<Postgres>,
}

impl TestDb {
    /// Starts a new PostgreSQL container, runs migrations, and returns a TestDb.
    pub async fn new() -> Self {
        let container = Postgres::default()
            .start()
            .await
            .expect("Failed to start PostgreSQL container");

        let host = container.get_host().await.expect("Failed to get host");
        let port = container
            .get_host_port_ipv4(5432)
            .await
            .expect("Failed to get port");

        let database_url = format!("postgres://postgres:postgres@{}:{}/postgres", host, port);

        let pool = create_pool(&database_url)
            .await
            .expect("Failed to create pool");

        run_migrations(&pool)
            .await
            .expect("Failed to run migrations");

        Self {
            pool,
            _container: container,
        }
    }
}
