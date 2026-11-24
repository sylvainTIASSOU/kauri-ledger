//! HTTP boundary for authenticated, tenant-scoped ledger operations.
//! These routes must only use a restricted ledger_app database pool.

use axum::{
    Json,
    body::{Body, Bytes},
    extract::{OriginalUri, Path, State},
    http::{HeaderMap, StatusCode, header::CONTENT_TYPE},
    response::Response,
};
use chrono::{DateTime, Utc};
use kauri_ledger_core::{Amount, Direction};
use kauri_ledger_store::{
    StoreError,
    api_auth::{ApiAccess, ApiKeyProof, ApiScope, check_api_access},
    authorized_repository::{
        get_authorized_balance, get_authorized_transaction, post_authorized_transaction,
    },
    idempotency::compute_request_hash,
    tenant_repository::{TenantLedgerEntry, TenantPosting},
};
use serde::Deserialize;
use serde_json::Value;
use sqlx::PgPool;
use utoipa::ToSchema;
use uuid::Uuid;

use crate::error::AppError;

#[derive(Clone)]
pub struct LedgerApiState {
    pub pool: PgPool,
}

#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
struct PostingEntry {
    account_id: Uuid,
    side: String,
    amount: i64,
}

#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
struct PostingBody {
    entries: Vec<PostingEntry>,
    effective_at: Option<DateTime<Utc>>,
}

fn public_error(status: StatusCode, code: &'static str, message: &'static str) -> AppError {
    AppError::public(status, code, message)
}

fn invalid_request() -> AppError {
    public_error(
        StatusCode::BAD_REQUEST,
        "VALIDATION_ERROR",
        "The request is invalid",
    )
}

fn not_found() -> AppError {
    public_error(
        StatusCode::NOT_FOUND,
        "NOT_FOUND",
        "The requested resource was not found",
    )
}

async fn authorize(
    pool: &PgPool,
    headers: &HeaderMap,
    ledger_id: Uuid,
    scope: ApiScope,
) -> Result<ApiKeyProof, AppError> {
    let value = headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| {
            public_error(
                StatusCode::UNAUTHORIZED,
                "UNAUTHENTICATED",
                "A valid bearer API key is required",
            )
        })?;

    let proof = ApiKeyProof::from_authorization(value).map_err(|_| {
        public_error(
            StatusCode::UNAUTHORIZED,
            "UNAUTHENTICATED",
            "A valid bearer API key is required",
        )
    })?;

    match check_api_access(pool, &proof, ledger_id, scope).await {
        Ok(ApiAccess::Allowed) => Ok(proof),
        Ok(ApiAccess::Unauthenticated) => Err(public_error(
            StatusCode::UNAUTHORIZED,
            "UNAUTHENTICATED",
            "A valid bearer API key is required",
        )),
        Ok(ApiAccess::NotFound) => Err(not_found()),
        Ok(ApiAccess::Forbidden) => Err(public_error(
            StatusCode::FORBIDDEN,
            "INSUFFICIENT_SCOPE",
            "The API key lacks the required scope",
        )),
        Err(_) => Err(AppError::internal()),
    }
}

fn store_error(error: StoreError) -> AppError {
    if let StoreError::Database(sqlx::Error::Database(db_error)) = &error {
        let message = db_error.message();
        let sqlstate = db_error.code();
        return match sqlstate.as_deref() {
            Some("28000") => public_error(
                StatusCode::UNAUTHORIZED,
                "UNAUTHENTICATED",
                "A valid bearer API key is required",
            ),
            Some("P0002") => not_found(),
            Some("42501") => public_error(
                StatusCode::FORBIDDEN,
                "INSUFFICIENT_SCOPE",
                "The API key lacks the required scope",
            ),
            Some("55P03") => public_error(
                StatusCode::CONFLICT,
                "CONCURRENT_REQUEST",
                "The request is blocked by a concurrent operation",
            ),
            Some("23503") => public_error(
                StatusCode::NOT_FOUND,
                "ACCOUNT_NOT_FOUND",
                "One of the requested accounts does not exist",
            ),
            Some("23514") if message.starts_with("IDEMPOTENCY_KEY_REUSED") => public_error(
                StatusCode::UNPROCESSABLE_ENTITY,
                "IDEMPOTENCY_KEY_REUSED",
                "This idempotency key was used with a different request",
            ),
            Some("23514")
                if message.starts_with("INSUFFICIENT_FUNDS")
                    || matches!(
                        db_error.constraint(),
                        Some(
                            "account_balances_available_nonnegative"
                                | "account_balances_posted_nonnegative"
                        )
                    ) =>
            {
                public_error(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "INSUFFICIENT_FUNDS",
                    "The transaction has insufficient funds",
                )
            }
            Some("23514") if message.starts_with("LEDGER_FROZEN") => public_error(
                StatusCode::CONFLICT,
                "LEDGER_FROZEN",
                "The ledger is frozen",
            ),
            Some("23514") if message.starts_with("PERIOD_LOCKED") => public_error(
                StatusCode::CONFLICT,
                "PERIOD_LOCKED",
                "The accounting period is closed",
            ),
            Some("23514") if message.starts_with("ACCOUNT_CLOSED") => public_error(
                StatusCode::CONFLICT,
                "ACCOUNT_CLOSED",
                "An account is closed",
            ),
            Some("23514") if message.starts_with("UNBALANCED_TRANSACTION") => public_error(
                StatusCode::BAD_REQUEST,
                "UNBALANCED_TRANSACTION",
                "Debit and credit totals must match",
            ),
            Some("23514") => invalid_request(),
            Some("40P01" | "40001" | "57014") => public_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "DATABASE_BUSY",
                "The ledger is temporarily busy",
            ),
            _ => AppError::internal(),
        };
    }

    match error {
        StoreError::InvalidRequestJson(_) | StoreError::InvalidRouteScope => invalid_request(),
        _ => AppError::internal(),
    }
}

fn json_response(body: Vec<u8>, status: StatusCode) -> Response {
    let mut response = Response::new(Body::from(body));
    *response.status_mut() = status;
    response
        .headers_mut()
        .insert(CONTENT_TYPE, "application/json".parse().unwrap());
    response
}

fn depth_is_valid(value: &Value, depth: usize) -> bool {
    if depth > 32 {
        return false;
    }
    match value {
        Value::Array(items) => items.iter().all(|item| depth_is_valid(item, depth + 1)),
        Value::Object(items) => items.values().all(|item| depth_is_valid(item, depth + 1)),
        _ => true,
    }
}

#[utoipa::path(
    post,
    path = "/v1/ledgers/{ledger_id}/transactions",
    tag = "ledger",
    params(
        (
            "ledger_id" = Uuid,
            Path,
            description = "Ledger identifier"
        ),
        (
            "Idempotency-Key" = String,
            Header,
            description = "Unique request key, 8 to 200 characters"
        )
    ),
    request_body(
        content = PostingBody,
        content_type = "application/json",
        description = "Balanced accounting entries"
    ),
    responses(
        (
            status = 201,
            description = "Transaction posted or idempotently replayed"
        ),
        (
            status = 400,
            description = "Invalid or unbalanced request"
        ),
        (
            status = 401,
            description = "Invalid or missing API key"
        ),
        (
            status = 403,
            description = "Insufficient API key scope"
        ),
        (
            status = 404,
            description = "Ledger not accessible"
        ),
        (
            status = 409,
            description = "Concurrent request or ledger conflict"
        ),
        (
            status = 422,
            description = "Idempotency conflict or insufficient funds"
        )
    ),
    security(("bearer_auth" = []))
)]
pub async fn post_transaction(
    State(state): State<LedgerApiState>,
    Path(ledger_id): Path<Uuid>,
    OriginalUri(original_uri): OriginalUri,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, AppError> {
    let proof = authorize(&state.pool, &headers, ledger_id, ApiScope::Write).await?;
    let path = format!("/v1/ledgers/{ledger_id}/transactions");

    if original_uri.path() != path
        || body.len() > 256 * 1024
        || headers.get(CONTENT_TYPE).and_then(|v| v.to_str().ok()) != Some("application/json")
    {
        return Err(invalid_request());
    }

    let key = headers
        .get("idempotency-key")
        .and_then(|value| value.to_str().ok())
        .filter(|value| (8..=200).contains(&value.chars().count()))
        .ok_or_else(invalid_request)?;

    // Reject duplicate JSON keys and non-I-JSON numbers before deserializing.
    compute_request_hash("POST", &path, &body).map_err(|_| invalid_request())?;

    let document: Value = serde_json::from_slice(&body).map_err(|_| invalid_request())?;
    if !depth_is_valid(&document, 1) {
        return Err(invalid_request());
    }
    let input: PostingBody = serde_json::from_value(document).map_err(|_| invalid_request())?;

    if !(2..=64).contains(&input.entries.len()) {
        return Err(invalid_request());
    }

    let effective_at = input.effective_at.unwrap_or_else(Utc::now);
    let mut entries = Vec::with_capacity(input.entries.len());
    let mut debit_total = 0_i128;
    let mut credit_total = 0_i128;

    for item in input.entries {
        if !(1..=1_000_000_000_000).contains(&item.amount) {
            return Err(invalid_request());
        }

        let direction = match item.side.as_str() {
            "D" => Direction::Debit,
            "C" => Direction::Credit,
            _ => return Err(invalid_request()),
        };
        match direction {
            Direction::Debit => debit_total += i128::from(item.amount),
            Direction::Credit => credit_total += i128::from(item.amount),
        }

        entries.push(TenantLedgerEntry {
            account_id: item.account_id,
            direction,
            amount: Amount::new(item.amount).map_err(|_| invalid_request())?,
        });
    }

    if debit_total != credit_total {
        return Err(public_error(
            StatusCode::BAD_REQUEST,
            "UNBALANCED_TRANSACTION",
            "Debit and credit totals must match",
        ));
    }

    let route_scope = format!("POST {path}");
    let request = TenantPosting {
        ledger_id,
        transaction_id: Uuid::now_v7(),
        route_scope: &route_scope,
        idempotency_key: key,
        raw_request_body: &body,
        response_body: b"{}",
        effective_at,
        entries: &entries,
    };

    let posted = post_authorized_transaction(&state.pool, &proof, &request)
        .await
        .map_err(store_error)?;

    // Both the original posting and its replay return the stored bytes.
    Ok(json_response(posted.response_body, StatusCode::CREATED))
}

#[utoipa::path(
    get,
    path = "/v1/ledgers/{ledger_id}/accounts/{code}/balance",
    tag = "ledger",
    params(
        (
            "ledger_id" = Uuid,
            Path,
            description = "Ledger identifier"
        ),
        (
            "code" = String,
            Path,
            description = "Account code"
        )
    ),
    responses(
        (
            status = 200,
            description = "Account balance in decimal string amounts"
        ),
        (
            status = 401,
            description = "Invalid or missing API key"
        ),
        (
            status = 403,
            description = "Insufficient API key scope"
        ),
        (
            status = 404,
            description = "Ledger or account not found"
        )
    ),
    security(("bearer_auth" = []))
)]
pub async fn account_balance(
    State(state): State<LedgerApiState>,
    Path((ledger_id, account_code)): Path<(Uuid, String)>,
    headers: HeaderMap,
) -> Result<Json<Value>, AppError> {
    let proof = authorize(&state.pool, &headers, ledger_id, ApiScope::Read).await?;

    let balance = get_authorized_balance(&state.pool, &proof, ledger_id, &account_code)
        .await
        .map_err(store_error)?
        .ok_or_else(not_found)?;

    Ok(Json(
        serde_json::to_value(balance).map_err(|_| AppError::internal())?,
    ))
}

#[utoipa::path(
    get,
    path = "/v1/ledgers/{ledger_id}/transactions/{transaction_id}",
    tag = "ledger",
    params(
        (
            "ledger_id" = Uuid,
            Path,
            description = "Ledger identifier"
        ),
        (
            "transaction_id" = Uuid,
            Path,
            description = "Transaction identifier"
        )
    ),
    responses(
        (
            status = 200,
            description = "Persisted transaction response"
        ),
        (
            status = 401,
            description = "Invalid or missing API key"
        ),
        (
            status = 403,
            description = "Insufficient API key scope"
        ),
        (
            status = 404,
            description = "Ledger or transaction not found"
        )
    ),
    security(("bearer_auth" = []))
)]
pub async fn transaction_by_id(
    State(state): State<LedgerApiState>,
    Path((ledger_id, transaction_id)): Path<(Uuid, Uuid)>,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    let proof = authorize(&state.pool, &headers, ledger_id, ApiScope::Read).await?;

    let transaction = get_authorized_transaction(&state.pool, &proof, ledger_id, transaction_id)
        .await
        .map_err(store_error)?
        .ok_or_else(not_found)?;

    Ok(json_response(transaction.response_body, StatusCode::OK))
}
