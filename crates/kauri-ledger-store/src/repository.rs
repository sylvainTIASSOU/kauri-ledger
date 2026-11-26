//! Legacy single-tenant accounting repository.
//!
//! Retained for compatibility and trusted maintenance workflows.
//! Never use this repository from tenant-facing HTTP handlers.
//! New accounting operations must use the tenant-authorized engine.

use sha2::{Digest, Sha256};
use sqlx::PgPool;
use uuid::Uuid;

use kauri_ledger_core::{AccountCode, AccountType, BalancedTransaction, Direction};

use crate::error::{StoreError, is_deadlock_error, is_unique_violation};
use crate::models::{DbAccountType, DbBalance, DbEntry, DbTransaction};

/// Maximum number of retries on deadlock errors
const MAX_DEADLOCK_RETRIES: u32 = 5;

/// Computes a SHA−256 hash of the transaction payload for idempotency conflict
fn compute_payload_hash(tx: &BalancedTransaction) -> String {
    let mut hasher = Sha256::new();
    for entry in tx.entries() {
        hasher.update(entry.account_code.as_str().as_bytes());
        hasher.update(match entry.direction {
            Direction::Debit => b"D",
            Direction::Credit => b"C",
        });
        hasher.update(entry.amount.as_i64().to_be_bytes());
    }
    hex::encode(hasher.finalize())
}

/// convert a core AccountType to its database representation.
fn to_db_account_type(account_type: AccountType) -> DbAccountType {
    match account_type {
        AccountType::Asset => DbAccountType::Asset,
        AccountType::Liability => DbAccountType::Liability,
        AccountType::Equity => DbAccountType::Equity,
        AccountType::Revenue => DbAccountType::Revenue,
        AccountType::Expense => DbAccountType::Expense,
    }
}

/// Creates a new account in the database.
pub async fn create_account(
    pool: &PgPool,
    code: &AccountCode,
    account_type: AccountType,
) -> Result<(), StoreError> {
    let db_type = to_db_account_type(account_type);
    sqlx::query(
        r#"
        INSERT INTO public.accounts (code, account_type)
        VALUES ($1, $2)
        "#,
    )
    .bind(code.as_str())
    .bind(db_type)
    .execute(pool)
    .await?;
    Ok(())
}

/// Records a balanced transaction atomically.
///
/// Guarantees:
/// 1. Idempotence: same key returns the same transaction.
/// 2. Atomicity: all writes happen in a single SQL transaction.
/// 3. Ordered locking: accounts are locked in lexicographic order to prevent deadlocks.
/// 4. Retry: up to MAX_DEADLOCK_RETRIES attempts on deadlock.
///
pub async fn record_transaction(
    pool: &PgPool,
    idempotency_key: &str,
    tx: &BalancedTransaction,
) -> Result<Uuid, StoreError> {
    let payload_hash = compute_payload_hash(tx);

    // Step 1: Check idempotency (fast path)
    let existing: Option<DbTransaction> = sqlx::query_as(
        r#"
        SELECT id, idempotency_key, payload_hash, created_at
        FROM public.transactions
        WHERE idempotency_key = $1
        "#,
    )
    .bind(idempotency_key)
    .fetch_optional(pool)
    .await?;

    if let Some(existing_tx) = existing {
        if existing_tx.payload_hash != payload_hash {
            return Err(StoreError::IdempotencyConflict);
        }
        return Ok(existing_tx.id);
    }

    // Step 2: Attempt to insert with retry on deadlock
    for attempt in 0..MAX_DEADLOCK_RETRIES {
        match try_record_transaction(pool, idempotency_key, tx, &payload_hash).await {
            Ok(tx_id) => return Ok(tx_id),
            Err(StoreError::Database(err)) if is_deadlock_error(&err) => {
                if attempt + 1 == MAX_DEADLOCK_RETRIES {
                    return Err(StoreError::MaxRetriesExceeded(MAX_DEADLOCK_RETRIES));
                }
                // Exponential backoff: 1ms, 2ms, 4ms, 8ms, ...
                let delay_ms = 2u64.pow(attempt);
                tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                continue;
            }
            Err(StoreError::Database(err)) if is_unique_violation(&err) => {
                // Another concurrent request inserted the same idempotency key.
                // Fetch it and verify the hash.
                let existing_tx: DbTransaction = sqlx::query_as(
                    r#"
                    SELECT id, idempotency_key, payload_hash, created_at
                    FROM transactions
                    WHERE idempotency_key = $1
                    "#,
                )
                .bind(idempotency_key)
                .fetch_one(pool)
                .await?;

                if existing_tx.payload_hash != payload_hash {
                    return Err(StoreError::IdempotencyConflict);
                }
                return Ok(existing_tx.id);
            }
            Err(e) => return Err(e),
        }
    }

    Err(StoreError::MaxRetriesExceeded(MAX_DEADLOCK_RETRIES))
}

/// Internal function that performs the actual transaction in a single SQL transaction.
async fn try_record_transaction(
    pool: &PgPool,
    idempotency_key: &str,
    tx: &BalancedTransaction,
    payload_hash: &str,
) -> Result<Uuid, StoreError> {
    let mut db_tx = pool.begin().await?;

    // Insert the transaction record
    let tx_id: Uuid = sqlx::query_scalar(
        r#"
        INSERT INTO public.transactions (idempotency_key, payload_hash)
        VALUES ($1, $2)
        RETURNING id
        "#,
    )
    .bind(idempotency_key)
    .bind(payload_hash)
    .fetch_one(&mut *db_tx)
    .await?;

    // Collect unique account codes and sort them lexicographically for ordered locking
    let mut account_codes: Vec<String> = tx
        .entries()
        .iter()
        .map(|e| e.account_code.as_str().to_string())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    account_codes.sort();

    // Lock balances in ordered fashion (prevents deadlocks)
    // Using INSERT ... ON CONFLICT DO NOTHING to ensure rows exist, then SELECT FOR UPDATE
    for code in &account_codes {
        sqlx::query(
            r#"
            INSERT INTO public.balances (account_code, balance, version)
            VALUES ($1, 0, 0)
            ON CONFLICT (account_code) DO NOTHING
            "#,
        )
        .bind(code)
        .execute(&mut *db_tx)
        .await?;
    }

    // Now lock them in order
    for code in &account_codes {
        sqlx::query(
            r#"
            SELECT balance, version FROM balances
            WHERE account_code = $1
            FOR UPDATE
            "#,
        )
        .bind(code)
        .fetch_one(&mut *db_tx)
        .await?;
    }

    // Insert entries and update balances
    for entry in tx.entries() {
        let delta = match entry.direction {
            Direction::Debit => entry.amount.as_i64(),
            Direction::Credit => -entry.amount.as_i64(),
        };

        sqlx::query(
            r#"
            INSERT INTO public.entries (transaction_id, account_code, direction, amount)
            VALUES ($1, $2, $3, $4)
            "#,
        )
        .bind(tx_id)
        .bind(entry.account_code.as_str())
        .bind(match entry.direction {
            Direction::Debit => "Debit",
            Direction::Credit => "Credit",
        })
        .bind(entry.amount.as_i64())
        .execute(&mut *db_tx)
        .await?;

        sqlx::query(
            r#"
            UPDATE public.balances
            SET balance = balance + $1,
                version = version + 1,
                updated_at = NOW()
            WHERE account_code = $2
            "#,
        )
        .bind(delta)
        .bind(entry.account_code.as_str())
        .execute(&mut *db_tx)
        .await?;
    }

    db_tx.commit().await?;
    Ok(tx_id)
}

/// Retrieves the current balance of an account.
pub async fn get_balance(
    pool: &PgPool,
    code: &AccountCode,
) -> Result<Option<DbBalance>, StoreError> {
    let balance: Option<DbBalance> = sqlx::query_as(
        r#"
        SELECT account_code, balance, version, updated_at
        FROM public.balances
        WHERE account_code = $1
        "#,
    )
    .bind(code.as_str())
    .fetch_optional(pool)
    .await?;
    Ok(balance)
}

/// Retrieves the entry history of an account, ordered by creation time.
pub async fn get_account_history(
    pool: &PgPool,
    code: &AccountCode,
) -> Result<Vec<DbEntry>, StoreError> {
    let entries: Vec<DbEntry> = sqlx::query_as(
        r#"
        SELECT id, transaction_id, account_code, direction, amount, created_at
        FROM public.entries
        WHERE account_code = $1
        ORDER BY created_at ASC, id ASC
        "#,
    )
    .bind(code.as_str())
    .fetch_all(pool)
    .await?;
    Ok(entries)
}

/// Calculates the actual balance of an account by summing all its entries.
/// This is used for integrity verification.
pub async fn calculate_actual_balance(
    pool: &PgPool,
    code: &AccountCode,
) -> Result<i64, StoreError> {
    let result: Option<(i64,)> = sqlx::query_as(
        r#"
        SELECT COALESCE(SUM(
            CASE
                WHEN direction = 'Debit' THEN amount
                ELSE -amount
            END
        ), 0)::BIGINT
        FROM entries
        WHERE account_code = $1
        "#,
    )
    .bind(code.as_str())
    .fetch_optional(pool)
    .await?;

    Ok(result.map(|(sum,)| sum).unwrap_or(0))
}

/// Verifies the integrity of an account's balance.
/// Returns Ok(()) if the stored balance matches the calculated balance.
/// Returns Err if there's a mismatch.
pub async fn verify_account_balance_integrity(
    pool: &PgPool,
    code: &AccountCode,
) -> Result<(), StoreError> {
    let stored_balance = get_balance(pool, code).await?;
    let actual_balance = calculate_actual_balance(pool, code).await?;

    match stored_balance {
        Some(db_balance) => {
            if db_balance.balance != actual_balance {
                Err(StoreError::BalanceIntegrityError {
                    account_code: code.as_str().to_string(),
                    stored: db_balance.balance,
                    actual: actual_balance,
                })
            } else {
                Ok(())
            }
        }
        None => {
            // If no balance row exists but there are entries, that's an integrity error
            if actual_balance != 0 {
                Err(StoreError::BalanceIntegrityError {
                    account_code: code.as_str().to_string(),
                    stored: 0,
                    actual: actual_balance,
                })
            } else {
                Ok(())
            }
        }
    }
}
