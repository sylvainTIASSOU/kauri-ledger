//! Tenant-authorized database operations for the HTTP runtime.
//! Never substitute a migration-owner pool for the restricted runtime pool.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use chrono::{DateTime, Utc};
use serde::{Serialize, Serializer};
use sqlx::{FromRow, PgPool};
use uuid::Uuid;

use crate::{
    api_auth::ApiKeyProof,
    error::StoreError,
    idempotency::compute_request_hash,
    tenant_repository::{TenantPosting, TenantPostingResult},
};

const MAX_ATTEMPTS: usize = 3;

fn serialize_i64_as_string<S>(value: &i64, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    serializer.serialize_str(&value.to_string())
}

#[derive(Debug, Clone, Serialize, FromRow)]
pub struct AuthorizedBalance {
    pub account_id: Uuid,
    pub account_code: String,
    pub account_status: String,
    #[serde(serialize_with = "serialize_i64_as_string")]
    pub posted_total: i64,
    #[serde(serialize_with = "serialize_i64_as_string")]
    pub reserved_total: i64,
    #[serde(serialize_with = "serialize_i64_as_string")]
    pub available_total: i64,
    pub balance_version: i64,
}

#[derive(Debug, Clone, FromRow)]
pub struct AuthorizedTransaction {
    pub transaction_id: Uuid,
    pub posted_at: DateTime<Utc>,
    pub effective_at: DateTime<Utc>,
    pub response_body: Vec<u8>,
}

pub async fn get_authorized_balance(
    pool: &PgPool,
    proof: &ApiKeyProof,
    ledger_id: Uuid,
    account_code: &str,
) -> Result<Option<AuthorizedBalance>, StoreError> {
    let balance = sqlx::query_as::<_, AuthorizedBalance>(
        r#"
        SELECT account_id, account_code, account_status,
               posted_total, reserved_total, available_total, balance_version
        FROM ledger.authorized_get_balance($1, $2, $3, $4)
        "#,
    )
    .bind(proof.prefix())
    .bind(proof.hash_bytes())
    .bind(ledger_id)
    .bind(account_code)
    .fetch_optional(pool)
    .await?;

    Ok(balance)
}

pub async fn get_authorized_transaction(
    pool: &PgPool,
    proof: &ApiKeyProof,
    ledger_id: Uuid,
    transaction_id: Uuid,
) -> Result<Option<AuthorizedTransaction>, StoreError> {
    let transaction = sqlx::query_as::<_, AuthorizedTransaction>(
        r#"
        SELECT transaction_id, posted_at, effective_at, response_body
        FROM ledger.authorized_get_transaction($1, $2, $3, $4)
        "#,
    )
    .bind(proof.prefix())
    .bind(proof.hash_bytes())
    .bind(ledger_id)
    .bind(transaction_id)
    .fetch_optional(pool)
    .await?;

    Ok(transaction)
}

fn retryable(error: &StoreError) -> bool {
    if let StoreError::Database(sqlx::Error::Database(database_error)) = error {
        return database_error
            .code()
            .is_some_and(|code| matches!(code.as_ref(), "40P01" | "40001"));
    }
    false
}

pub async fn post_authorized_transaction(
    pool: &PgPool,
    proof: &ApiKeyProof,
    request: &TenantPosting<'_>,
) -> Result<TenantPostingResult, StoreError> {
    for attempt in 0..MAX_ATTEMPTS {
        match post_once(pool, proof, request).await {
            Err(error) if retryable(&error) && attempt + 1 < MAX_ATTEMPTS => {
                let jitter = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .subsec_nanos() as u64
                    % 16;
                let delay = 25 * (1u64 << attempt) + jitter;
                tokio::time::sleep(Duration::from_millis(delay)).await;
            }
            result => return result,
        }
    }
    unreachable!("The bounded retry loop must return")
}

async fn post_once(
    pool: &PgPool,
    proof: &ApiKeyProof,
    request: &TenantPosting<'_>,
) -> Result<TenantPostingResult, StoreError> {
    let request_path = format!("/v1/ledgers/{}/transactions", request.ledger_id);
    let route_scope = format!("POST {request_path}");
    if request.route_scope != route_scope {
        return Err(StoreError::InvalidRouteScope);
    }

    let request_hash = compute_request_hash("POST", &request_path, request.raw_request_body)?;

    let account_ids: Vec<Uuid> = request
        .entries
        .iter()
        .map(|entry| entry.account_id)
        .collect();
    let sides: Vec<String> = request
        .entries
        .iter()
        .map(|entry| match entry.direction {
            kauri_ledger_core::Direction::Debit => "D".to_owned(),
            kauri_ledger_core::Direction::Credit => "C".to_owned(),
        })
        .collect();
    let amounts: Vec<i64> = request
        .entries
        .iter()
        .map(|entry| entry.amount.as_i64())
        .collect();

    let mut transaction = pool.begin().await?;
    sqlx::query("SET LOCAL lock_timeout = '3s'")
        .execute(&mut *transaction)
        .await?;
    sqlx::query("SET LOCAL statement_timeout = '5s'")
        .execute(&mut *transaction)
        .await?;

    // The SQL wrapper generates and persists the server-owned response bytes.
    // The caller-supplied response_body is intentionally never used here.
    let row: (Uuid, bool, Vec<u8>) = sqlx::query_as(
        r#"
        SELECT posted_transaction_id, replayed, stored_response_body
        FROM ledger.authorized_post_transaction(
            $1, $2, $3, $4, $5, $6, $7, $8, $9,
            (
                SELECT COALESCE(
                    jsonb_agg(jsonb_build_object(
                        'account_id', e.account_id,
                        'side', e.side,
                        'amount', e.amount
                    ) ORDER BY e.position),
                    '[]'::jsonb
                )
                FROM unnest($10::uuid[], $11::text[], $12::bigint[])
                     WITH ORDINALITY AS e(account_id, side, amount, position)
            )
        )
        "#,
    )
    .bind(proof.prefix())
    .bind(proof.hash_bytes())
    .bind(request.ledger_id)
    .bind(request.transaction_id)
    .bind(request.route_scope)
    .bind(request.idempotency_key)
    .bind(request_hash.to_vec())
    .bind(&b"{}"[..])
    .bind(request.effective_at)
    .bind(account_ids)
    .bind(sides)
    .bind(amounts)
    .fetch_one(&mut *transaction)
    .await?;

    transaction.commit().await?;

    Ok(TenantPostingResult {
        transaction_id: row.0,
        replayed: row.1,
        response_body: row.2,
    })
}

/// Reject accidental use of privileged migration credentials by the HTTP API.
/// The runtime login must inherit only ledger_app's required privileges.
pub async fn assert_restricted_runtime_pool(pool: &PgPool) -> Result<(), StoreError> {
    let (is_superuser, can_create_role, bypasses_rls, can_create_db): (bool, bool, bool, bool) =
        sqlx::query_as(
            "SELECT rolsuper, rolcreaterole, rolbypassrls, rolcreatedb \
         FROM pg_catalog.pg_roles WHERE rolname = current_user",
        )
        .fetch_one(pool)
        .await?;

    let (can_read_keys, can_write_transactions, can_execute_wrapper):
        (bool, bool, bool) = sqlx::query_as(
        r#"
        SELECT
            has_table_privilege(current_user, 'ledger.api_keys', 'SELECT'),
            has_table_privilege(current_user, 'ledger.transactions', 'INSERT'),
            has_function_privilege(
                current_user,
                'ledger.authorized_post_transaction(text,bytea,uuid,uuid,text,text,bytea,bytea,timestamptz,jsonb)',
                'EXECUTE'
            )
        "#,
    )
    .fetch_one(pool)
    .await?;

    if is_superuser
        || can_create_role
        || bypasses_rls
        || can_create_db
        || can_read_keys
        || can_write_transactions
        || !can_execute_wrapper
    {
        return Err(StoreError::UnsafeRuntimeRole);
    }
    Ok(())
}
