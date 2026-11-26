mod common;

use chrono::Utc;
use kauri_ledger_core::{AccountCode, AccountType, Amount, Direction};
use kauri_ledger_store::{
    api_auth::ApiKeyProof,
    idempotency::compute_request_hash,
    integrity::{LedgerStructuralIssue, verify_ledger_integrity},
    tenant_repository::{
        TenantLedgerEntry, TenantPosting, create_ledger_account, post_tenant_transaction,
    },
};
use serde_json::Value;
use sha2::{Digest, Sha256};
use sqlx::{Error as SqlxError, PgPool};
use uuid::Uuid;

type ReversalRow = (Uuid, bool, Vec<u8>);
type JournalRow = (Uuid, String, i64);
type BalanceState = (Uuid, i64, i64, i64, i64);
type LedgerSnapshot = (Vec<BalanceState>, (i64, i64, i64));
/// PostgreSQL backend activity used for lock contention diagnostics.
///
/// Fields: process ID, state, wait event type,
/// wait event name, and blocking process IDs.
type BackendActivity = (
    i32,
    Option<String>,
    Option<String>,
    Option<String>,
    Vec<i32>,
);
async fn create_ledger(pool: &PgPool, name: &str) -> Uuid {
    let ledger_id = Uuid::now_v7();

    sqlx::query(
        "INSERT INTO ledger.ledgers (id, name) \
         VALUES ($1, $2)",
    )
    .bind(ledger_id)
    .bind(name)
    .execute(pool)
    .await
    .unwrap();

    ledger_id
}

async fn create_account(pool: &PgPool, ledger_id: Uuid, code: &str, allow_negative: bool) -> Uuid {
    let account_id = Uuid::now_v7();

    create_ledger_account(
        pool,
        ledger_id,
        account_id,
        &AccountCode::new(code).unwrap(),
        AccountType::Asset,
        code,
        allow_negative,
    )
    .await
    .unwrap();

    account_id
}

async fn create_write_key(pool: &PgPool, ledger_id: Uuid) -> ApiKeyProof {
    let token = format!("lk_test_{}", "R".repeat(43));
    let key_hash: [u8; 32] = Sha256::digest(token.as_bytes()).into();

    let key_id = Uuid::now_v7();

    sqlx::query(
        r#"
        INSERT INTO ledger.api_keys (
            id, key_hash, prefix, last4, scopes, label
        )
        VALUES ($1, $2, $3, $4, $5, $6)
        "#,
    )
    .bind(key_id)
    .bind(key_hash.as_slice())
    .bind(&token[..16])
    .bind(&token[token.len() - 4..])
    .bind(vec!["write"])
    .bind("Reversal integration test")
    .execute(pool)
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO ledger.api_key_ledgers \
         (api_key_id, ledger_id) VALUES ($1, $2)",
    )
    .bind(key_id)
    .bind(ledger_id)
    .execute(pool)
    .await
    .unwrap();

    ApiKeyProof::from_authorization(&format!("Bearer {token}")).unwrap()
}

async fn post_original(
    pool: &PgPool,
    ledger_id: Uuid,
    wallet_id: Uuid,
    funding_id: Uuid,
    idempotency_key: &str,
) -> Uuid {
    let entries = [
        TenantLedgerEntry {
            account_id: wallet_id,
            direction: Direction::Debit,
            amount: Amount::new(100).unwrap(),
        },
        TenantLedgerEntry {
            account_id: funding_id,
            direction: Direction::Credit,
            amount: Amount::new(100).unwrap(),
        },
    ];

    let body = format!(
        r#"{{"entries":[{{"account_id":"{wallet_id}","side":"D","amount":100}},{{"account_id":"{funding_id}","side":"C","amount":100}}]}}"#
    );

    let route_scope = format!("POST /v1/ledgers/{ledger_id}/transactions");

    let request = TenantPosting {
        ledger_id,
        transaction_id: Uuid::now_v7(),
        route_scope: &route_scope,
        idempotency_key,
        response_body: b"{}",
        raw_request_body: body.as_bytes(),
        effective_at: Utc::now(),
        entries: &entries,
    };

    post_tenant_transaction(pool, &request)
        .await
        .unwrap()
        .transaction_id
}

async fn load_entries(pool: &PgPool, ledger_id: Uuid, transaction_id: Uuid) -> Vec<JournalRow> {
    sqlx::query_as(
        r#"
        SELECT account_id, side::text, amount
        FROM ledger.entries
        WHERE ledger_id = $1
          AND transaction_id = $2
        ORDER BY id
        "#,
    )
    .bind(ledger_id)
    .bind(transaction_id)
    .fetch_all(pool)
    .await
    .unwrap()
}

/// Execute the reversal under the restricted ledger_app role.
///
/// The SQL transaction must commit before the test reports success:
/// deferred accounting checks and the transaction seal run at COMMIT.
async fn reverse_as_app(
    pool: &PgPool,
    proof: &ApiKeyProof,
    ledger_id: Uuid,
    original_id: Uuid,
    reversal_id: Uuid,
    idempotency_key: &str,
    request_hash: &[u8],
) -> Result<ReversalRow, SqlxError> {
    let mut transaction = pool.begin().await?;

    sqlx::query("SET LOCAL ROLE ledger_app")
        .execute(&mut *transaction)
        .await?;

    let result = sqlx::query_as::<_, ReversalRow>(
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
    .bind(ledger_id)
    .bind(original_id)
    .bind(reversal_id)
    .bind(idempotency_key)
    .bind(request_hash)
    .bind(Utc::now())
    .fetch_one(&mut *transaction)
    .await;

    match result {
        Ok(row) => {
            transaction.commit().await?;
            Ok(row)
        }
        Err(error) => {
            let _ = transaction.rollback().await;
            Err(error)
        }
    }
}

fn assert_database_error(error: &SqlxError, expected_state: &str, expected_message: &str) {
    let SqlxError::Database(database_error) = error else {
        panic!("Expected a PostgreSQL error, got {error:?}");
    };

    assert_eq!(database_error.code().as_deref(), Some(expected_state));

    assert!(
        database_error.message().starts_with(expected_message),
        "Unexpected database error: {error}"
    );
}

fn reversal_hash(ledger_id: Uuid, original_id: Uuid) -> [u8; 32] {
    let path = format!("/v1/ledgers/{ledger_id}/transactions/{original_id}/reverse");

    compute_request_hash("POST", &path, b"{}").unwrap()
}

/// Capture accounting state to detect unintended side effects.
///
/// This includes cumulative totals, reservations, balance versions
/// and the number of transactions, entries and validation seals.
///
/// The helper is used after concurrent operations have completed.
async fn accounting_snapshot(pool: &PgPool, ledger_id: Uuid) -> LedgerSnapshot {
    let balances: Vec<BalanceState> = sqlx::query_as(
        r#"
        SELECT
            account_id,
            debit_total,
            credit_total,
            reserved_total,
            version
        FROM ledger.account_balances
        WHERE ledger_id = $1
        ORDER BY account_id
        "#,
    )
    .bind(ledger_id)
    .fetch_all(pool)
    .await
    .unwrap();

    let counts: (i64, i64, i64) = sqlx::query_as(
        r#"
        SELECT
            (
                SELECT COUNT(*)
                FROM ledger.transactions
                WHERE ledger_id = $1
            ),
            (
                SELECT COUNT(*)
                FROM ledger.entries
                WHERE ledger_id = $1
            ),
            (
                SELECT COUNT(*)
                FROM ledger.transaction_seals
                WHERE ledger_id = $1
            )
        "#,
    )
    .bind(ledger_id)
    .fetch_one(pool)
    .await
    .unwrap();

    (balances, counts)
}

#[tokio::test]
async fn reversal_is_atomic_idempotent_and_tenant_scoped() {
    let db = common::TestDb::new().await;

    let ledger_id = create_ledger(&db.pool, "Reversal Ledger A").await;

    let foreign_ledger = create_ledger(&db.pool, "Reversal Ledger B").await;

    let wallet_id = create_account(&db.pool, ledger_id, "asset:wallet", false).await;

    let funding_id = create_account(&db.pool, ledger_id, "asset:funding", true).await;

    let proof = create_write_key(&db.pool, ledger_id).await;

    // Post the original transaction through the existing engine.
    let original_id = post_original(
        &db.pool,
        ledger_id,
        wallet_id,
        funding_id,
        "reversal-source-001",
    )
    .await;

    let original_entries = load_entries(&db.pool, ledger_id, original_id).await;

    assert_eq!(original_entries.len(), 2);

    // The first reversal must create a new immutable transaction.
    let reversal_id = Uuid::now_v7();
    let hash = reversal_hash(ledger_id, original_id);

    let first = reverse_as_app(
        &db.pool,
        &proof,
        ledger_id,
        original_id,
        reversal_id,
        "reverse-source-001",
        &hash,
    )
    .await
    .unwrap();

    assert_eq!(first.0, reversal_id);
    assert!(!first.1);

    let response: Value = serde_json::from_slice(&first.2).unwrap();

    assert_eq!(response["status"], "posted");
    assert_eq!(response["transaction_id"], serde_json::json!(reversal_id));
    assert_eq!(
        response["reverses_transaction"],
        serde_json::json!(original_id)
    );

    let reversal_entries = load_entries(&db.pool, ledger_id, reversal_id).await;

    // Each entry must be an exact opposite of the original.
    assert_eq!(original_entries.len(), reversal_entries.len());

    for (original, reversed) in original_entries.iter().zip(reversal_entries.iter()) {
        assert_eq!(original.0, reversed.0);
        assert_eq!(original.2, reversed.2);

        let expected_side = if original.1 == "D" { "C" } else { "D" };

        assert_eq!(reversed.1, expected_side);
    }

    // The original immutable journal is not modified.
    assert_eq!(
        load_entries(&db.pool, ledger_id, original_id).await,
        original_entries
    );

    let parent: Option<Uuid> = sqlx::query_scalar(
        r#"
        SELECT reverses_transaction
        FROM ledger.transactions
        WHERE ledger_id = $1 AND id = $2
        "#,
    )
    .bind(ledger_id)
    .bind(reversal_id)
    .fetch_one(&db.pool)
    .await
    .unwrap();

    assert_eq!(parent, Some(original_id));

    // Original + reversal must restore the initial wallet balance.
    let (debit, credit): (i64, i64) = sqlx::query_as(
        r#"
        SELECT debit_total, credit_total
        FROM ledger.account_balances
        WHERE ledger_id = $1 AND account_id = $2
        "#,
    )
    .bind(ledger_id)
    .bind(wallet_id)
    .fetch_one(&db.pool)
    .await
    .unwrap();

    assert_eq!(debit, 100);
    assert_eq!(credit, 100);

    // Same request key and same payload hash return identical bytes.
    let replay = reverse_as_app(
        &db.pool,
        &proof,
        ledger_id,
        original_id,
        Uuid::now_v7(),
        "reverse-source-001",
        &hash,
    )
    .await
    .unwrap();

    assert_eq!(replay.0, reversal_id);
    assert!(replay.1);
    assert_eq!(replay.2, first.2);

    // A different request hash must not reuse the same key.
    let changed_hash = [0xA5_u8; 32];

    let conflict = reverse_as_app(
        &db.pool,
        &proof,
        ledger_id,
        original_id,
        Uuid::now_v7(),
        "reverse-source-001",
        &changed_hash,
    )
    .await
    .unwrap_err();

    assert_database_error(&conflict, "23514", "IDEMPOTENCY_KEY_REUSED");

    // Another idempotency key cannot reverse the same original.
    let duplicate = reverse_as_app(
        &db.pool,
        &proof,
        ledger_id,
        original_id,
        Uuid::now_v7(),
        "reverse-source-002",
        &hash,
    )
    .await
    .unwrap_err();

    assert_database_error(&duplicate, "23514", "ALREADY_REVERSED");

    // A key for ledger A must not access ledger B.
    let foreign = reverse_as_app(
        &db.pool,
        &proof,
        foreign_ledger,
        original_id,
        Uuid::now_v7(),
        "foreign-reverse-001",
        &hash,
    )
    .await
    .unwrap_err();

    assert_database_error(&foreign, "P0002", "LEDGER_NOT_FOUND");

    // A reversal is itself a posted transaction and may be reversed.
    let second_reversal_id = Uuid::now_v7();
    let second_hash = reversal_hash(ledger_id, reversal_id);

    let second = reverse_as_app(
        &db.pool,
        &proof,
        ledger_id,
        reversal_id,
        second_reversal_id,
        "reverse-reversal-001",
        &second_hash,
    )
    .await
    .unwrap();

    assert_eq!(second.0, second_reversal_id);
    assert!(!second.1);

    // Every committed transaction must have a validation seal.
    let seal_count: i64 = sqlx::query_scalar(
        r#"
        SELECT COUNT(*)
        FROM ledger.transaction_seals
        WHERE ledger_id = $1
        "#,
    )
    .bind(ledger_id)
    .fetch_one(&db.pool)
    .await
    .unwrap();

    assert_eq!(seal_count, 3);

    // The full journal remains internally consistent.
    let report = verify_ledger_integrity(&db.pool, ledger_id)
        .await
        .unwrap()
        .unwrap();

    assert!(report.is_healthy());

    // A previously committed request can replay after a freeze.
    sqlx::query(
        "UPDATE ledger.ledgers \
         SET status = 'frozen' WHERE id = $1",
    )
    .bind(ledger_id)
    .execute(&db.pool)
    .await
    .unwrap();

    let frozen_replay = reverse_as_app(
        &db.pool,
        &proof,
        ledger_id,
        original_id,
        Uuid::now_v7(),
        "reverse-source-001",
        &hash,
    )
    .await
    .unwrap();

    assert!(frozen_replay.1);
    assert_eq!(frozen_replay.2, first.2);

    // A new reversal is forbidden while the ledger is frozen.
    let new_hash = reversal_hash(ledger_id, second_reversal_id);

    let frozen_new = reverse_as_app(
        &db.pool,
        &proof,
        ledger_id,
        second_reversal_id,
        Uuid::now_v7(),
        "reverse-frozen-001",
        &new_hash,
    )
    .await
    .unwrap_err();

    assert_database_error(&frozen_new, "23514", "LEDGER_FROZEN");
}

#[tokio::test]
async fn concurrent_reversals_create_only_one_compensation() {
    let db = common::TestDb::new().await;

    let ledger_id = create_ledger(&db.pool, "Concurrent Reversal Ledger").await;

    let wallet_id = create_account(&db.pool, ledger_id, "asset:wallet", false).await;

    let funding_id = create_account(&db.pool, ledger_id, "asset:funding", true).await;

    let proof = create_write_key(&db.pool, ledger_id).await;

    let original_id = post_original(
        &db.pool,
        ledger_id,
        wallet_id,
        funding_id,
        "concurrent-source-001",
    )
    .await;

    let hash = reversal_hash(ledger_id, original_id);

    // Two concurrent requests represent the same logical operation.
    let (first, second) = tokio::join!(
        reverse_as_app(
            &db.pool,
            &proof,
            ledger_id,
            original_id,
            Uuid::now_v7(),
            "concurrent-reversal-001",
            &hash,
        ),
        reverse_as_app(
            &db.pool,
            &proof,
            ledger_id,
            original_id,
            Uuid::now_v7(),
            "concurrent-reversal-001",
            &hash,
        ),
    );

    let first = first.unwrap();
    let second = second.unwrap();

    // Exactly one request creates the reversal.
    // The other must replay the committed result.
    assert_eq!(first.0, second.0);
    assert_ne!(first.1, second.1);
    assert_eq!(first.2, second.2);

    let reversal_id = first.0;

    let snapshot = accounting_snapshot(&db.pool, ledger_id).await;

    // One original and one reversal, each with two entries.
    assert_eq!(snapshot.1, (2, 4, 2));

    let second_hash = reversal_hash(ledger_id, reversal_id);

    // Different idempotency keys represent different requests
    // attempting to reverse the same source transaction.
    let (first, second) = tokio::join!(
        reverse_as_app(
            &db.pool,
            &proof,
            ledger_id,
            reversal_id,
            Uuid::now_v7(),
            "concurrent-distinct-001",
            &second_hash,
        ),
        reverse_as_app(
            &db.pool,
            &proof,
            ledger_id,
            reversal_id,
            Uuid::now_v7(),
            "concurrent-distinct-002",
            &second_hash,
        ),
    );

    match (first, second) {
        (Ok(winner), Err(loser)) | (Err(loser), Ok(winner)) => {
            assert!(!winner.1);

            assert_database_error(&loser, "23514", "ALREADY_REVERSED");
        }

        other => {
            panic!(
                "Expected one successful reversal \
                 and one conflict, got {other:?}"
            );
        }
    }

    // Exactly three transactions must exist:
    // original, reversal and reversal of the reversal.
    let snapshot = accounting_snapshot(&db.pool, ledger_id).await;

    assert_eq!(snapshot.1, (3, 6, 3));

    let wallet_balance: i64 = sqlx::query_scalar(
        r#"
        SELECT debit_total - credit_total
        FROM ledger.account_balances
        WHERE ledger_id = $1
          AND account_id = $2
        "#,
    )
    .bind(ledger_id)
    .bind(wallet_id)
    .fetch_one(&db.pool)
    .await
    .unwrap();

    assert_eq!(wallet_balance, 100);

    let report = verify_ledger_integrity(&db.pool, ledger_id)
        .await
        .unwrap()
        .unwrap();

    assert!(report.is_healthy());
}

#[tokio::test]
async fn reversal_rejections_leave_accounting_state_unchanged() {
    let db = common::TestDb::new().await;

    let ledger_id = create_ledger(&db.pool, "Reversal Rollback Ledger").await;

    let wallet_id = create_account(&db.pool, ledger_id, "asset:wallet", false).await;

    let funding_id = create_account(&db.pool, ledger_id, "asset:funding", true).await;

    let proof = create_write_key(&db.pool, ledger_id).await;

    // Fund the wallet with 100 XOF.
    let original_id = post_original(
        &db.pool,
        ledger_id,
        wallet_id,
        funding_id,
        "rollback-funding-001",
    )
    .await;

    // Spend the entire wallet balance through a regular posting.
    // Swapping the accounts reverses the direction of the movement.
    post_original(
        &db.pool,
        ledger_id,
        funding_id,
        wallet_id,
        "rollback-spending-001",
    )
    .await;

    let before = accounting_snapshot(&db.pool, ledger_id).await;

    assert_eq!(before.1, (2, 4, 2));

    let hash = reversal_hash(ledger_id, original_id);

    // Reversing the initial funding would overdraw the wallet.
    let rejected = reverse_as_app(
        &db.pool,
        &proof,
        ledger_id,
        original_id,
        Uuid::now_v7(),
        "rollback-reversal-001",
        &hash,
    )
    .await
    .unwrap_err();

    let SqlxError::Database(database_error) = &rejected else {
        panic!("Expected a balance constraint error: {rejected}");
    };

    assert_eq!(database_error.code().as_deref(), Some("23514"),);

    assert!(matches!(
        database_error.constraint(),
        Some("account_balances_posted_nonnegative" | "account_balances_available_nonnegative")
    ));

    // No balance, version, transaction, entry or seal may change.
    let after = accounting_snapshot(&db.pool, ledger_id).await;

    assert_eq!(before, after);

    // Fund the wallet again so insufficient funds cannot mask
    // the accounting period validation in the next scenario.
    post_original(
        &db.pool,
        ledger_id,
        wallet_id,
        funding_id,
        "rollback-funding-002",
    )
    .await;

    // Use a future cutoff only for this isolated test.
    // This guarantees that the reversal's effective date is locked.
    let cutoff = Utc::now() + chrono::Duration::hours(1);

    sqlx::query("SELECT ledger.close_period($1, $2)")
        .bind(ledger_id)
        .bind(cutoff)
        .execute(&db.pool)
        .await
        .unwrap();

    let before_period_failure = accounting_snapshot(&db.pool, ledger_id).await;

    let period_error = reverse_as_app(
        &db.pool,
        &proof,
        ledger_id,
        original_id,
        Uuid::now_v7(),
        "rollback-period-001",
        &hash,
    )
    .await
    .unwrap_err();

    assert_database_error(&period_error, "23514", "PERIOD_LOCKED");

    let after_period_failure = accounting_snapshot(&db.pool, ledger_id).await;

    assert_eq!(before_period_failure, after_period_failure,);

    let report = verify_ledger_integrity(&db.pool, ledger_id)
        .await
        .unwrap()
        .unwrap();

    assert!(report.is_healthy());
}

#[tokio::test]
async fn rejects_balanced_but_inexact_reversal_at_commit() {
    let db = common::TestDb::new().await;

    let ledger_id = create_ledger(&db.pool, "Exact Reversal Constraint Ledger").await;

    let wallet_id = create_account(&db.pool, ledger_id, "asset:wallet", false).await;

    let funding_id = create_account(&db.pool, ledger_id, "asset:funding", true).await;

    let original_id = post_original(
        &db.pool,
        ledger_id,
        wallet_id,
        funding_id,
        "exact-source-001",
    )
    .await;

    let before = accounting_snapshot(&db.pool, ledger_id).await;

    let reversal_id = Uuid::now_v7();
    let effective_at = Utc::now();

    let mut transaction = db.pool.begin().await.unwrap();

    // Simulate an incorrectly implemented privileged posting path.
    // This intentionally bypasses the authorized reversal wrapper.
    sqlx::query(
        r#"
        INSERT INTO ledger.transactions (
            ledger_id,
            id,
            route_scope,
            idempotency_key,
            request_hash,
            response_snapshot,
            response_body,
            reverses_transaction,
            effective_at
        )
        VALUES (
            $1,
            $2,
            $3,
            $4,
            $5,
            '{}'::jsonb,
            $6,
            $7,
            $8
        )
        "#,
    )
    .bind(ledger_id)
    .bind(reversal_id)
    .bind(format!(
        "POST /v1/ledgers/{ledger_id}/transactions/{original_id}/reverse"
    ))
    .bind("inexact-reversal-001")
    .bind(reversal_hash(ledger_id, original_id).to_vec())
    .bind(&b"{}"[..])
    .bind(original_id)
    .bind(effective_at)
    .execute(&mut *transaction)
    .await
    .unwrap();

    // The original contains one debit of 100 and one credit of 100.
    //
    // This false reversal splits the required credit of 100
    // into two credits of 50. It is balanced and has identical
    // aggregate account deltas, but is not an exact reversal.
    for (account_id, side, amount) in [
        (wallet_id, "C", 50_i64),
        (wallet_id, "C", 50_i64),
        (funding_id, "D", 100_i64),
    ] {
        sqlx::query(
            r#"
            INSERT INTO ledger.entries (
                ledger_id,
                transaction_id,
                account_id,
                side,
                amount,
                effective_at
            )
            VALUES ($1, $2, $3, $4, $5, $6)
            "#,
        )
        .bind(ledger_id)
        .bind(reversal_id)
        .bind(account_id)
        .bind(side)
        .bind(amount)
        .bind(effective_at)
        .execute(&mut *transaction)
        .await
        .unwrap();
    }

    // Apply otherwise valid cumulative balance changes.
    // The exact reversal constraint must still reject the commit.
    sqlx::query(
        r#"
        UPDATE ledger.account_balances
        SET
            debit_total = debit_total + CASE
                WHEN account_id = $3 THEN 100
                ELSE 0
            END,
            credit_total = credit_total + CASE
                WHEN account_id = $2 THEN 100
                ELSE 0
            END,
            version = version + 1,
            updated_at = NOW()
        WHERE ledger_id = $1
          AND account_id IN ($2, $3)
        "#,
    )
    .bind(ledger_id)
    .bind(wallet_id)
    .bind(funding_id)
    .execute(&mut *transaction)
    .await
    .unwrap();

    // All individual SQL statements succeeded.
    // The independent deferred constraint must reject COMMIT.
    let error = transaction
        .commit()
        .await
        .expect_err("An inexact reversal must not commit");

    assert_database_error(&error, "23514", "INVALID_REVERSAL_ENTRIES");

    let after = accounting_snapshot(&db.pool, ledger_id).await;

    assert_eq!(before, after);

    let report = verify_ledger_integrity(&db.pool, ledger_id)
        .await
        .unwrap()
        .unwrap();

    assert!(report.is_healthy());
}

#[tokio::test]
async fn audit_detects_invalid_reversal_relationship() {
    let db = common::TestDb::new().await;

    let ledger_id = create_ledger(&db.pool, "Reversal Audit Ledger").await;

    let wallet_id = create_account(&db.pool, ledger_id, "asset:wallet", false).await;

    let funding_id = create_account(&db.pool, ledger_id, "asset:funding", true).await;

    let original_id = post_original(
        &db.pool,
        ledger_id,
        wallet_id,
        funding_id,
        "audit-source-001",
    )
    .await;

    // Post another transaction in the same direction.
    // This is a valid posting, but not a reversal.
    let second_id = post_original(
        &db.pool,
        ledger_id,
        wallet_id,
        funding_id,
        "audit-source-002",
    )
    .await;

    let healthy_report = verify_ledger_integrity(&db.pool, ledger_id)
        .await
        .unwrap()
        .unwrap();

    assert!(healthy_report.is_healthy());

    let before = accounting_snapshot(&db.pool, ledger_id).await;

    // Simulate privileged database corruption.
    // This bypass is restricted to the isolated test container.
    let mut transaction = db.pool.begin().await.unwrap();

    sqlx::query("SET LOCAL session_replication_role = 'replica'")
        .execute(&mut *transaction)
        .await
        .unwrap();

    sqlx::query(
        r#"
        UPDATE ledger.transactions
        SET reverses_transaction = $3
        WHERE ledger_id = $1
          AND id = $2
        "#,
    )
    .bind(ledger_id)
    .bind(second_id)
    .bind(original_id)
    .execute(&mut *transaction)
    .await
    .unwrap();

    transaction.commit().await.unwrap();

    // The corruption changes the reversal relationship,
    // not the accounting amounts or journal entry counts.
    let after = accounting_snapshot(&db.pool, ledger_id).await;

    assert_eq!(before, after);

    let report = verify_ledger_integrity(&db.pool, ledger_id)
        .await
        .unwrap()
        .unwrap();

    // All financial totals still agree.
    assert_eq!(report.journal_net_total, 0);
    assert_eq!(report.stored_net_total, 0);
    assert!(report.mismatches.is_empty());

    // But the reversal relationship is invalid.
    assert!(!report.is_healthy());

    assert_eq!(
        report.structural_issues,
        vec![LedgerStructuralIssue::InvalidReversal {
            transaction_id: second_id,
            original_transaction_id: original_id,
        }],
    );
}

#[tokio::test]
async fn mixed_posting_paths_wait_for_the_same_idempotency_lock() {
    let db = common::TestDb::new().await;

    // Ensure the test is running against the unified posting engine.
    let migration_applied: bool = sqlx::query_scalar(
        r#"
        SELECT COUNT(*) = 2
        FROM public._sqlx_migrations
        WHERE version IN (18, 19)
          AND success = TRUE
        "#,
    )
    .fetch_one(&db.pool)
    .await
    .unwrap();

    assert!(
        migration_applied,
        "Migrations 018 and 019 must be applied before testing posting contention"
    );

    let ledger_id = create_ledger(&db.pool, "Mixed Posting Lock Ledger").await;

    let wallet_id = create_account(&db.pool, ledger_id, "asset:wallet", false).await;

    let funding_id = create_account(&db.pool, ledger_id, "asset:funding", true).await;

    let proof = create_write_key(&db.pool, ledger_id).await;

    let path = format!("/v1/ledgers/{ledger_id}/transactions");

    let route_scope = format!("POST {path}");
    let idempotency_key = "mixed-posting-001";

    let entries = serde_json::json!([
        {
            "account_id": wallet_id,
            "side": "D",
            "amount": 100
        },
        {
            "account_id": funding_id,
            "side": "C",
            "amount": 100
        }
    ]);

    let raw_body = serde_json::json!({
        "entries": &entries
    })
    .to_string();

    let entries_json = entries.to_string();

    let hash = compute_request_hash("POST", &path, raw_body.as_bytes()).unwrap();

    // The trusted posting path creates a transaction but
    // deliberately keeps it uncommitted.
    let mut first = db.pool.begin().await.unwrap();

    let first_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *first)
        .await
        .unwrap();

    let transaction_id = Uuid::now_v7();

    // Preserve the trusted caller's response contract.
    let response = br#"{"status":"trusted-post"}"#;

    let created: (Uuid, bool, Vec<u8>) = sqlx::query_as(
        r#"
        SELECT
            posted_transaction_id,
            replayed,
            stored_response_body
        FROM ledger.post_transaction(
            $1, $2, $3, $4, $5, $6, $7, $8::jsonb
        )
        "#,
    )
    .bind(ledger_id)
    .bind(transaction_id)
    .bind(&route_scope)
    .bind(idempotency_key)
    .bind(hash.as_slice())
    .bind(response.as_slice())
    .bind(Utc::now())
    .bind(entries_json.as_str())
    .fetch_one(&mut *first)
    .await
    .unwrap();

    assert!(!created.1);
    assert_eq!(created.0, transaction_id);

    // Inspect the advisory locks from the posting connection itself.
    let first_holds_lock: bool = sqlx::query_scalar(
        r#"
        SELECT EXISTS (
            SELECT 1
            FROM pg_catalog.pg_locks
            WHERE pid = pg_backend_pid()
              AND locktype = 'advisory'
              AND granted = TRUE
        )
        "#,
    )
    .fetch_one(&mut *first)
    .await
    .unwrap();

    if !first_holds_lock {
        // Inspect the function definitions actually installed
        // in the test database, not just the migration history.
        let definitions: Vec<(String, String)> = sqlx::query_as(
            r#"
            SELECT
                p.oid::regprocedure::text,
                pg_get_functiondef(p.oid)
            FROM pg_catalog.pg_proc AS p
            JOIN pg_catalog.pg_namespace AS n
              ON n.oid = p.pronamespace
            WHERE n.nspname = 'ledger'
              AND p.proname IN (
                  'execute_posting',
                  'post_transaction',
                  'authorized_post_transaction',
                  'authorized_reverse_transaction'
              )
            ORDER BY p.proname, p.oid
            "#,
        )
        .fetch_all(&mut *first)
        .await
        .unwrap();

        for (signature, definition) in definitions {
            eprintln!("\nFunction: {signature}\n{definition}\n");
        }

        panic!(
            "The trusted posting path did not acquire \
             an advisory lock. Inspect the installed \
             PostgreSQL function definitions above."
        );
    }

    // A separate connection uses the authorized HTTP posting path.
    let mut second = db.pool.begin().await.unwrap();

    sqlx::query("SET LOCAL ROLE ledger_app")
        .execute(&mut *second)
        .await
        .unwrap();

    sqlx::query("SET LOCAL statement_timeout = '15s'")
        .execute(&mut *second)
        .await
        .unwrap();

    let second_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *second)
        .await
        .unwrap();
    let replayed = {
        let replay_query = sqlx::query_as::<_, (Uuid, bool, Vec<u8>)>(
            r#"
        SELECT
            posted_transaction_id,
            replayed,
            stored_response_body
        FROM ledger.authorized_post_transaction(
            $1, $2, $3, $4, $5,
            $6, $7, $8, $9, $10::jsonb
        )
        "#,
        )
        .bind(proof.prefix())
        .bind(proof.hash_bytes())
        .bind(ledger_id)
        .bind(Uuid::now_v7())
        .bind(&route_scope)
        .bind(idempotency_key)
        .bind(hash.as_slice())
        .bind(&b"{}"[..])
        .bind(Utc::now())
        .bind(entries_json.as_str())
        .fetch_one(&mut *second);

        tokio::pin!(replay_query);

        // Observe a real pending advisory lock.
        // Fail immediately if the SQL query completes unexpectedly.
        let observation = tokio::select! {
            result = &mut replay_query => {
                panic!(
                    "The second posting completed before the \
                     first transaction committed: {result:?}"
                );
            }

            result = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                async {
                    loop {
                        let waiting: bool = sqlx::query_scalar(
                            r#"
                        SELECT
                            $1::integer = ANY(
                                pg_blocking_pids($2::integer)
                            )
                            AND EXISTS (
                                SELECT 1
                                FROM pg_catalog.pg_locks
                                WHERE pid = $2
                                  AND locktype = 'advisory'
                                  AND granted = FALSE
                            )
                        "#,
                        )
                        .bind(first_pid)
                        .bind(second_pid)
                        .fetch_one(&db.pool)
                        .await
                        .unwrap();

                        if waiting {
                            break;
                        }

                        tokio::time::sleep(
                            std::time::Duration::from_millis(20),
                        )
                        .await;
                    }
                },
            ) => result,
        };

        // Collect useful PostgreSQL diagnostics instead of
        // reporting an unexplained timeout.
        if observation.is_err() {
            let activity: Vec<BackendActivity> = sqlx::query_as(
                r#"
            SELECT
                pid,
                state,
                wait_event_type,
                wait_event,
                pg_blocking_pids(pid)
            FROM pg_catalog.pg_stat_activity
            WHERE pid IN ($1, $2)
            ORDER BY pid
            "#,
            )
            .bind(first_pid)
            .bind(second_pid)
            .fetch_all(&db.pool)
            .await
            .unwrap();

            let locks: Vec<(i32, String, String, bool)> = sqlx::query_as(
                r#"
                SELECT
                    pid,
                    locktype,
                    mode,
                    granted
                FROM pg_catalog.pg_locks
                WHERE pid IN ($1, $2)
                ORDER BY pid, locktype
                "#,
            )
            .bind(first_pid)
            .bind(second_pid)
            .fetch_all(&db.pool)
            .await
            .unwrap();

            panic!(
                "Expected advisory lock contention.\n\
             First PID: {first_pid}\n\
             Second PID: {second_pid}\n\
             Activity: {activity:#?}\n\
             Locks: {locks:#?}"
            );
        }

        // Release the advisory lock by committing the winner.
        first.commit().await.unwrap();

        // Resume the same pending query.
        replay_query.await.unwrap()
    };

    second.commit().await.unwrap();

    assert!(replayed.1);
    assert_eq!(replayed.0, transaction_id);
    assert_eq!(replayed.2, created.2);

    // The shared engine must apply the operation only once.
    let snapshot = accounting_snapshot(&db.pool, ledger_id).await;

    assert_eq!(snapshot.1, (1, 2, 1));

    let balance: (i64, i64, i64) = sqlx::query_as(
        r#"
        SELECT debit_total, credit_total, version
        FROM ledger.account_balances
        WHERE ledger_id = $1
          AND account_id = $2
        "#,
    )
    .bind(ledger_id)
    .bind(wallet_id)
    .fetch_one(&db.pool)
    .await
    .unwrap();

    assert_eq!(balance, (100, 0, 1));

    let report = verify_ledger_integrity(&db.pool, ledger_id)
        .await
        .unwrap()
        .unwrap();

    assert!(report.is_healthy());
}

#[tokio::test]
async fn concurrent_posting_and_reversal_cannot_overdraw_wallet() {
    let db = common::TestDb::new().await;

    let ledger_id = create_ledger(&db.pool, "Mixed Accounting Concurrency Ledger").await;

    let wallet_id = create_account(&db.pool, ledger_id, "asset:wallet", false).await;

    let funding_id = create_account(&db.pool, ledger_id, "asset:funding", true).await;

    let proof = create_write_key(&db.pool, ledger_id).await;

    let original_id = post_original(
        &db.pool,
        ledger_id,
        wallet_id,
        funding_id,
        "mixed-funding-001",
    )
    .await;

    let before = accounting_snapshot(&db.pool, ledger_id).await;

    assert_eq!(before.1, (1, 2, 1));

    let hash = reversal_hash(ledger_id, original_id);

    // A regular posting spends the full wallet balance.
    let entries = serde_json::json!([
        {
            "account_id": funding_id,
            "side": "D",
            "amount": 100
        },
        {
            "account_id": wallet_id,
            "side": "C",
            "amount": 100
        }
    ])
    .to_string();

    let route_scope = format!("POST /v1/ledgers/{ledger_id}/transactions");

    let spending = sqlx::query_as::<_, (Uuid, bool, Vec<u8>)>(
        r#"
        SELECT
            posted_transaction_id,
            replayed,
            stored_response_body
        FROM ledger.post_transaction(
            $1, $2, $3, $4, $5, $6, $7, $8::jsonb
        )
        "#,
    )
    .bind(ledger_id)
    .bind(Uuid::now_v7())
    .bind(&route_scope)
    .bind("mixed-spending-001")
    .bind(vec![42_u8; 32])
    .bind(&b"{}"[..])
    .bind(Utc::now())
    .bind(entries.as_str())
    .fetch_one(&db.pool);

    let reversal = reverse_as_app(
        &db.pool,
        &proof,
        ledger_id,
        original_id,
        Uuid::now_v7(),
        "mixed-reversal-001",
        &hash,
    );

    let (spending_result, reversal_result) = tokio::join!(spending, reversal);

    match (spending_result, reversal_result) {
        (Ok(winner), Err(loser)) | (Err(loser), Ok(winner)) => {
            assert!(!winner.1);

            let SqlxError::Database(database_error) = &loser else {
                panic!("Expected an insufficient funds error: {loser}");
            };

            assert_eq!(database_error.code().as_deref(), Some("23514"),);

            assert!(matches!(
                database_error.constraint(),
                Some(
                    "account_balances_posted_nonnegative"
                        | "account_balances_available_nonnegative"
                )
            ));
        }

        other => {
            panic!(
                "Expected exactly one successful operation, \
                 got {other:?}"
            );
        }
    }

    // The failed operation must leave no committed side effects.
    let snapshot = accounting_snapshot(&db.pool, ledger_id).await;

    assert_eq!(snapshot.1, (2, 4, 2));

    let wallet_totals: (i64, i64, i64) = sqlx::query_as(
        r#"
        SELECT debit_total, credit_total, version
        FROM ledger.account_balances
        WHERE ledger_id = $1
          AND account_id = $2
        "#,
    )
    .bind(ledger_id)
    .bind(wallet_id)
    .fetch_one(&db.pool)
    .await
    .unwrap();

    assert_eq!(wallet_totals, (100, 100, 2));

    let report = verify_ledger_integrity(&db.pool, ledger_id)
        .await
        .unwrap()
        .unwrap();

    assert!(report.is_healthy());
}
