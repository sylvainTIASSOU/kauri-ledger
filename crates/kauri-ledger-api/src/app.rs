use crate::config::settings::{Config, print_startup_banner};
use crate::config::{log, telemetry};
use crate::controllers::{health, hello};
use crate::error::normalize_errors;
use crate::openapi::ApiDoc;
use anyhow::{Context, Result};
use axum::middleware::from_fn;
use axum::{Router, http::HeaderName, routing::get};
use std::net::SocketAddr;
use tokio::net::TcpListener;
use tower_http::{
    request_id::{MakeRequestUuid, PropagateRequestIdLayer, SetRequestIdLayer},
    trace::TraceLayer,
};
use utoipa::OpenApi;
use utoipa_swagger_ui::SwaggerUi;

use crate::controllers::ledger::LedgerApiState;
use axum::{extract::DefaultBodyLimit, routing::post};
use kauri_ledger_store::{authorized_repository::assert_restricted_runtime_pool, create_pool};

fn base_routes() -> Router {
    Router::new()
        .route("/health", get(health::health_handler))
        .route("/hello", get(hello::hello_handler))
        .merge(SwaggerUi::new("/swagger-ui").url("/api-docs/openapi.json", ApiDoc::openapi()))
}

fn with_common_layers(router: Router) -> Router {
    let request_id_header = HeaderName::from_static("x-request-id");

    router
        .layer(from_fn(normalize_errors))
        .layer(PropagateRequestIdLayer::new(request_id_header.clone()))
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(telemetry::make_span_with)
                .on_response(telemetry::on_response)
                .on_failure(telemetry::on_failure),
        )
        .layer(SetRequestIdLayer::new(request_id_header, MakeRequestUuid))
}

/// Preserve the existing DB-free router for legacy unit tests.
pub fn build_router() -> Router {
    with_common_layers(base_routes())
}

pub fn build_router_with_ledger(state: LedgerApiState) -> Router {
    let ledger_routes = Router::new()
        .route(
            "/v1/ledgers/{ledger_id}/transactions",
            post(crate::controllers::ledger::post_transaction),
        )
        .route(
            "/v1/ledgers/{ledger_id}/accounts/{code}/balance",
            get(crate::controllers::ledger::account_balance),
        )
        .route(
            "/v1/ledgers/{ledger_id}/transactions/{transaction_id}",
            get(crate::controllers::ledger::transaction_by_id),
        )
        .layer(DefaultBodyLimit::max(256 * 1024))
        .with_state(state);

    with_common_layers(base_routes().merge(ledger_routes))
}

pub struct App;

impl App {
    pub async fn run() -> Result<()> {
        dotenvy::dotenv().ok();

        // Configuration errors are propagated to main.
        let config = Config::from_env()?;
        log::setup()?;

        let address = format!("{}:{}", config.host, config.port);

        let database_url = std::env::var("LEDGER_DATABASE_URL")
            .context("LEDGER_DATABASE_URL is required for the HTTP runtime")?;
        let pool = create_pool(&database_url)
            .await
            .context("Failed to connect to the restricted ledger database")?;
        assert_restricted_runtime_pool(&pool)
            .await
            .context("HTTP database credentials violate least-privilege requirements")?;

        let listener = TcpListener::bind(&address)
            .await
            .with_context(|| format!("Failed to bind server to {address}"))?;

        let local_address = listener
            .local_addr()
            .context("Failed to read the server local address")?;

        let display_host = match config.host.as_str() {
            // 0.0.0.0 is a listening address, while 127.0.0.1 is
            // more convenient for opening the application locally.
            "0.0.0.0" => "127.0.0.1",
            host => host,
        };

        let base_url = format!("http://{}:{}", display_host, local_address.port());

        print_startup_banner(&base_url);

        axum::serve(
            listener,
            build_router_with_ledger(LedgerApiState { pool })
                .into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .context("Axum server failed")?;

        Ok(())
    }
}
