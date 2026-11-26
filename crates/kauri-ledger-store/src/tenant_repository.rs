use chrono::{DateTime, Utc};
use kauri_ledger_core::{AccountCode, AccountType, Amount, Direction};
use sqlx::PgPool;
use std::time::Duration;
use uuid::Uuid;

use crate::error::StoreError;
use crate::idempotency::compute_request_hash;

/// An entry referencing an account by its tenant-scoped UUID.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TenantLedgerEntry {
    pub account_id: Uuid,
    pub direction: Direction,
    pub amount: Amount,
}

const MAX_POST_ATTEMPTS: usize = 3;
const RETRY_BASE_DELAY_MS: u64 = 25;

fn is_retryable_sqlstate(code: &str) -> bool {
    matches!(code, "40001" | "40P01")
}

fn is_retryable_posting_error(error: &StoreError) -> bool {
    match error {
        StoreError::Database(sqlx::Error::Database(database_error)) => database_error
            .code()
            .is_some_and(|code| is_retryable_sqlstate(code.as_ref())),
        _ => false,
    }
}

/// Input for a trusted ledger posting operation.
///
/// The caller must provide an authenticated ledger scope,
/// The trusted HTTP layer must supply the original route scope
/// and raw JSON body before applying server-side defaults.
/// Parsed entries must originate from that same body.
#[derive(Debug)]
pub struct TenantPosting<'a> {
    pub ledger_id: Uuid,
    pub transaction_id: Uuid,
    pub route_scope: &'a str,
    pub idempotency_key: &'a str,
    pub raw_request_body: &'a [u8],
    pub response_body: &'a [u8],
    pub effective_at: DateTime<Utc>,
    pub entries: &'a [TenantLedgerEntry],
}

/// Result of a committed posting or an idempotent replay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TenantPostingResult {
    pub transaction_id: Uuid,
    pub replayed: bool,
    pub response_body: Vec<u8>,
}

/// create an account and its initial balance atomically.
///
/// This function requires a trusted database connection.
/// Tenant authorization must be performed before invoking this function.
pub async fn create_ledger_account(
    pool: &PgPool,
    ledger_id: Uuid,
    account_id: Uuid,
    code: &AccountCode,
    account_type: AccountType,
    name: &str,
    allow_negative: bool,
) -> Result<Uuid, StoreError> {
    let kind = match account_type {
        AccountType::Asset => "asset",
        AccountType::Liability => "liability",
        AccountType::Equity => "equity",
        AccountType::Revenue => "revenue",
        AccountType::Expense => "expense",
    };

    let create_id: Uuid = sqlx::query_scalar(
        "SELECT ledger.create_account(\
        $1, $2, $3, $4, $5, $6\
        )",
    )
    .bind(ledger_id)
    .bind(account_id)
    .bind(code.as_str())
    .bind(kind)
    .bind(name)
    .bind(allow_negative)
    .fetch_one(pool)
    .await?;

    Ok(create_id)
}

/// Post with bounded retries for transient PostgreSQL failures.
///
/// Each attempt starts a new database transaction. Business and
/// permission errors are never retried.
pub async fn post_tenant_transaction(
    pool: &PgPool,
    request: &TenantPosting<'_>,
) -> Result<TenantPostingResult, StoreError> {
    let mut attempt = 0;

    loop {
        attempt += 1;

        match post_tenant_transaction_once(pool, request).await {
            Err(error) if attempt < MAX_POST_ATTEMPTS && is_retryable_posting_error(&error) => {
                let delay_ms = RETRY_BASE_DELAY_MS * (1_u64 << (attempt - 1));

                tokio::time::sleep(Duration::from_millis(delay_ms)).await;
            }
            result => return result,
        }
    }
}

/// Post a tenant-scoped transaction using the protected SQL function.
///
/// The PostgreSQL function is responsible for idempotency,
/// ordered balance locks, fund checks, entry insertion,
/// deferred balance validation, and transaction sealing.
///
/// This adapter must not be exposed directly to untrusted clients.
async fn post_tenant_transaction_once(
    pool: &PgPool,
    request: &TenantPosting<'_>,
) -> Result<TenantPostingResult, StoreError> {
    // Bind the idempotency scope to the canonical tenant route.
    let request_path = format!("/v1/ledgers/{}/transactions", request.ledger_id);

    let expected_route_scope = format!("POST {request_path}");

    if request.route_scope != expected_route_scope {
        return Err(StoreError::InvalidRouteScope);
    }

    // Hash the original JSON body before applying any defaults.
    let request_hash = compute_request_hash("POST", &request_path, request.raw_request_body)?;

    // Build three parallel arrays from the same validated input.
    let account_ids: Vec<Uuid> = request
        .entries
        .iter()
        .map(|entry| entry.account_id)
        .collect();

    let sides: Vec<String> = request
        .entries
        .iter()
        .map(|entry| match entry.direction {
            Direction::Debit => "D".to_owned(),
            Direction::Credit => "C".to_owned(),
        })
        .collect();

    let amounts: Vec<i64> = request
        .entries
        .iter()
        .map(|entry| entry.amount.as_i64())
        .collect();

    // Apply timeouts only to this posting transaction.
    let mut db_tx = pool.begin().await?;

    sqlx::query("SET LOCAL lock_timeout = '3s'")
        .execute(&mut *db_tx)
        .await?;

    sqlx::query("SET LOCAL statement_timeout = '5s'")
        .execute(&mut *db_tx)
        .await?;

    // Construct JSONB on the database side using typed parameters.
    // WITH ORDINALITY preserves the original entry order.
    let result: Result<(Uuid, bool, Vec<u8>), sqlx::Error> = sqlx::query_as(
        r#"
        SELECT
            posted_transaction_id,
            replayed,
            stored_response_body
        FROM ledger.post_transaction(
            $1,
            $2,
            $3,
            $4,
            $5,
            $6,
            $7,
            (
                SELECT COALESCE(
                    jsonb_agg(
                        jsonb_build_object(
                            'account_id', e.account_id,
                            'side', e.side,
                            'amount', e.amount
                        )
                        ORDER BY e.position
                    ),
                    '[]'::jsonb
                )
                FROM unnest(
                    $8::uuid[],
                    $9::text[],
                    $10::bigint[]
                ) WITH ORDINALITY
                    AS e(account_id, side, amount, position)
            )
        )
        "#,
    )
    .bind(request.ledger_id)
    .bind(request.transaction_id)
    .bind(request.route_scope)
    .bind(request.idempotency_key)
    .bind(request_hash.to_vec())
    .bind(request.response_body.to_vec())
    .bind(request.effective_at)
    .bind(account_ids)
    .bind(sides)
    .bind(amounts)
    .fetch_one(&mut *db_tx)
    .await;

    let (transaction_id, replayed, response_body) = match result {
        Ok(row) => row,
        Err(error) => {
            let _ = db_tx.rollback().await;
            return Err(StoreError::Database(error));
        }
    };

    // Deferred PostgreSQL checks execute before success is returned.
    db_tx.commit().await?;

    Ok(TenantPostingResult {
        transaction_id,
        replayed,
        response_body,
    })
}

#[cfg(test)]
mod retry_tests {
    use super::is_retryable_sqlstate;

    #[test]
    fn retries_only_deadlocks_and_serialization_failures() {
        assert!(is_retryable_sqlstate("40P01"));
        assert!(is_retryable_sqlstate("40001"));

        for code in ["55P03", "57014", "23514", "23505", "42501"] {
            assert!(
                !is_retryable_sqlstate(code),
                "Unexpected retry for SQLSTATE {code}"
            );
        }
    }
}
