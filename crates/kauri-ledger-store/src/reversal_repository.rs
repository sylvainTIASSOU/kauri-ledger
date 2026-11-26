use std::time::{Duration, SystemTime, UNIX_EPOCH};

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use crate::{
    api_auth::ApiKeyProof, error::StoreError, idempotency::compute_request_hash,
    tenant_repository::TenantPostingResult,
};

const MAX_REVERSAL_ATTEMPTS: usize = 3;
const RETRY_BASE_DELAY_MS: u64 = 25;

/// Input for an authenticated reversal operation.
///
/// The source transaction determines the accounting entries.
/// Callers cannot supply their own reversal amounts or accounts.
#[derive(Debug)]
pub struct AuthorizedReversal<'a> {
    pub ledger_id: Uuid,
    pub original_transaction_id: Uuid,
    pub reversal_transaction_id: Uuid,
    pub idempotency_key: &'a str,
    pub raw_request_body: &'a [u8],
    pub effective_at: DateTime<Utc>,
}

/// Only serialization failures and deadlocks may be retried.
///
/// Business errors must never be retried automatically.
fn is_retryable(error: &StoreError) -> bool {
    if let StoreError::Database(sqlx::Error::Database(database_error)) = error {
        return database_error
            .code()
            .is_some_and(|code| matches!(code.as_ref(), "40001" | "40P01"));
    }

    false
}

/// Execute a tenant-authorized reversal with bounded retries.
pub async fn reverse_authorized_transaction(
    pool: &PgPool,
    proof: &ApiKeyProof,
    request: &AuthorizedReversal<'_>,
) -> Result<TenantPostingResult, StoreError> {
    for attempt in 0..MAX_REVERSAL_ATTEMPTS {
        match reverse_once(pool, proof, request).await {
            Err(error) if is_retryable(&error) && attempt + 1 < MAX_REVERSAL_ATTEMPTS => {
                // Small timing variation reduces synchronized retries.
                let jitter = u64::from(
                    SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap_or_default()
                        .subsec_nanos(),
                ) % 16;

                let delay = RETRY_BASE_DELAY_MS * (1_u64 << attempt) + jitter;

                tokio::time::sleep(Duration::from_millis(delay)).await;
            }

            result => return result,
        }
    }

    unreachable!("The bounded retry loop must return")
}

/// One reversal attempt runs within a single SQL transaction.
async fn reverse_once(
    pool: &PgPool,
    proof: &ApiKeyProof,
    request: &AuthorizedReversal<'_>,
) -> Result<TenantPostingResult, StoreError> {
    let path = format!(
        "/v1/ledgers/{}/transactions/{}/reverse",
        request.ledger_id, request.original_transaction_id,
    );

    // Bind the hash to the real HTTP method, canonical route,
    // and raw JSON captured before server-side defaults.
    let request_hash = compute_request_hash("POST", &path, request.raw_request_body)?;

    let mut transaction = pool.begin().await?;

    // Prevent an HTTP request from holding accounting locks
    // indefinitely when another operation blocks progress.
    sqlx::query("SET LOCAL lock_timeout = '3s'")
        .execute(&mut *transaction)
        .await?;

    sqlx::query("SET LOCAL statement_timeout = '5s'")
        .execute(&mut *transaction)
        .await?;

    // The SECURITY DEFINER wrapper validates tenant access
    // and derives all reversed entries from the stored journal.
    let row: (Uuid, bool, Vec<u8>) = sqlx::query_as(
        r#"
        SELECT
            posted_transaction_id,
            replayed,
            stored_response_body
        FROM ledger.authorized_reverse_transaction(
            $1, $2, $3, $4, $5, $6, $7, $8
        )
        "#,
    )
    .bind(proof.prefix())
    .bind(proof.hash_bytes())
    .bind(request.ledger_id)
    .bind(request.original_transaction_id)
    .bind(request.reversal_transaction_id)
    .bind(request.idempotency_key)
    .bind(request_hash.as_slice())
    .bind(request.effective_at)
    .fetch_one(&mut *transaction)
    .await?;

    // Deferred triggers validate and seal the transaction here.
    // Never report success to HTTP before COMMIT succeeds.
    transaction.commit().await?;

    Ok(TenantPostingResult {
        transaction_id: row.0,
        replayed: row.1,
        response_body: row.2,
    })
}
