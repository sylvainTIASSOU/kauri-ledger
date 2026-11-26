use utoipa::Modify;
use utoipa::OpenApi;
use utoipa::openapi::security::{HttpAuthScheme, HttpBuilder, SecurityScheme};

struct BearerSecurity;

impl Modify for BearerSecurity {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        if let Some(components) = openapi.components.as_mut() {
            components.add_security_scheme(
                "bearer_auth",
                SecurityScheme::Http(HttpBuilder::new().scheme(HttpAuthScheme::Bearer).build()),
            );
        }
    }
}

#[derive(OpenApi)]
#[openapi(
    modifiers(&BearerSecurity),
    info(
        title = "kauri-ledger",
        version = "0.1.0",
        description = "Secure multi-tenant accounting API for Kauri Ledger"
    ),
    paths(
        crate::controllers::hello::hello_handler,
        crate::controllers::health::health_handler,
        crate::controllers::ledger::post_transaction,
        crate::controllers::ledger::reverse_transaction,
        crate::controllers::ledger::account_balance,
        crate::controllers::ledger::transaction_by_id,
    ),
    components(
        schemas(
            crate::models::message::MessageResponse,
            crate::models::message::HealthResponse,
            crate::error::ErrorResponse,
        crate::error::ErrorBody
        )
    ),
    tags(
        (
            name = "hello",
            description = "Greeting endpoints"
        ),
        (
            name = "health",
            description = "Application health endpoints"
        ),
        (
            name = "ledger",
            description = "Authenticated tenant ledger operations"
       )
    )
)]
pub struct ApiDoc;
