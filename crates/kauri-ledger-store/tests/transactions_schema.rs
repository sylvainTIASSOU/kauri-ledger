mod common;

use chrono::{DateTime, Utc};
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

fn assert_sqlstate(error: sqlx::Error, expected: &str) {
    let code = match error {
        sqlx::Error::Database(database_error) => {
            database_error.code().map(|code| code.into_owned())
        }
        other => panic!("Expected a PostgreSQL error, got {other:?}"),
    };

    assert_eq!(code.as_deref(), Some(expected));
}

async fn create_accounts(pool: &PgPool) -> (Uuid, Uuid, Uuid) {
    let ledger_id = Uuid::now_v7();
    let debit_id = Uuid::now_v7();
    let credit_id = Uuid::now_v7();

    sqlx::query("INSERT INTO ledger.ledgers (id, name) VALUES ($1, $2)")
        .bind(ledger_id)
        .bind("Transaction Test Ledger")
        .execute(pool)
        .await
        .unwrap();

    for (account_id, code, kind, normal_side) in [
        (debit_id, "asset:cash", "asset", "D"),
        (credit_id, "liability:debt", "liability", "C"),
    ] {
        sqlx::query(
            "INSERT INTO ledger.accounts \
             (ledger_id, id, code, kind, normal_side, name) \
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(ledger_id)
        .bind(account_id)
        .bind(code)
        .bind(kind)
        .bind(normal_side)
        .bind("Test Account")
        .execute(pool)
        .await
        .unwrap();

        sqlx::query(
            "INSERT INTO ledger.account_balances \
             (ledger_id, account_id, normal_side, allow_negative) \
             VALUES ($1, $2, $3, FALSE)",
        )
        .bind(ledger_id)
        .bind(account_id)
        .bind(normal_side)
        .execute(pool)
        .await
        .unwrap();
    }

    (ledger_id, debit_id, credit_id)
}

async fn insert_transaction(
    connection: &mut PgConnection,
    ledger_id: Uuid,
    transaction_id: Uuid,
    route_scope: &str,
    idempotency_key: &str,
    effective_at: DateTime<Utc>,
    reverses_transaction: Option<Uuid>,
) {
    sqlx::query(
        "INSERT INTO ledger.transactions \
         (ledger_id, id, route_scope, idempotency_key, \
          request_hash, response_snapshot, response_body, \
          effective_at, reverses_transaction) \
         VALUES ($1, $2, $3, $4, $5, '{}'::jsonb, \
                 $6, $7, $8)",
    )
    .bind(ledger_id)
    .bind(transaction_id)
    .bind(route_scope)
    .bind(idempotency_key)
    .bind(vec![7_u8; 32])
    .bind(b"{}".to_vec())
    .bind(effective_at)
    .bind(reverses_transaction)
    .execute(connection)
    .await
    .unwrap();
}

async fn post(
    pool: &PgPool,
    ledger_id: Uuid,
    debit_id: Uuid,
    credit_id: Uuid,
    route_scope: &str,
    idempotency_key: &str,
    reverses_transaction: Option<Uuid>,
) -> Uuid {
    let mut transaction = pool.begin().await.unwrap();

    let transaction_id = Uuid::now_v7();
    let effective_at = Utc::now();

    insert_transaction(
        &mut transaction,
        ledger_id,
        transaction_id,
        route_scope,
        idempotency_key,
        effective_at,
        reverses_transaction,
    )
    .await;

    for (account_id, side) in [(debit_id, "D"), (credit_id, "C")] {
        sqlx::query(
            "INSERT INTO ledger.entries \
             (ledger_id, transaction_id, account_id, \
              side, amount, effective_at) \
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(ledger_id)
        .bind(transaction_id)
        .bind(account_id)
        .bind(side)
        .bind(100_i64)
        .bind(effective_at)
        .execute(&mut *transaction)
        .await
        .unwrap();

        sqlx::query(
            "UPDATE ledger.account_balances \
             SET debit_total = debit_total + \
                 CASE WHEN $3 = 'D' THEN $4 ELSE 0 END, \
                 credit_total = credit_total + \
                 CASE WHEN $3 = 'C' THEN $4 ELSE 0 END \
             WHERE ledger_id = $1 AND account_id = $2",
        )
        .bind(ledger_id)
        .bind(account_id)
        .bind(side)
        .bind(100_i64)
        .execute(&mut *transaction)
        .await
        .unwrap();
    }

    transaction.commit().await.unwrap();

    transaction_id
}

#[tokio::test]
async fn creates_balanced_entries_with_valid_references() {
    let db = common::TestDb::new().await;
    let (ledger_id, debit_id, credit_id) = create_accounts(&db.pool).await;

    let transaction_id = post(
        &db.pool,
        ledger_id,
        debit_id,
        credit_id,
        "POST /transactions",
        "transaction-key-001",
        None,
    )
    .await;

    let (entry_count, net_amount): (i64, i64) = sqlx::query_as(
        "SELECT COUNT(*), \
             COALESCE(SUM(CASE WHEN side = 'D' \
                 THEN amount ELSE -amount END), 0)::BIGINT \
             FROM ledger.entries \
             WHERE ledger_id = $1 AND transaction_id = $2",
    )
    .bind(ledger_id)
    .bind(transaction_id)
    .fetch_one(&db.pool)
    .await
    .unwrap();

    assert_eq!(entry_count, 2);
    assert_eq!(net_amount, 0);

    let body: Vec<u8> = sqlx::query_scalar(
        "SELECT response_body FROM ledger.transactions \
         WHERE ledger_id = $1 AND id = $2",
    )
    .bind(ledger_id)
    .bind(transaction_id)
    .fetch_one(&db.pool)
    .await
    .unwrap();

    assert_eq!(body, b"{}".to_vec());
}

#[tokio::test]
async fn scopes_idempotency_by_route_and_limits_reversals() {
    let db = common::TestDb::new().await;
    let (ledger_id, debit_id, credit_id) = create_accounts(&db.pool).await;

    let original_id = post(
        &db.pool,
        ledger_id,
        debit_id,
        credit_id,
        "POST /transactions",
        "shared-key-001",
        None,
    )
    .await;

    post(
        &db.pool,
        ledger_id,
        debit_id,
        credit_id,
        "POST /another-route",
        "shared-key-001",
        None,
    )
    .await;

    let duplicate = sqlx::query(
        "INSERT INTO ledger.transactions \
         (ledger_id, id, route_scope, idempotency_key, \
          request_hash, response_snapshot, response_body, \
          effective_at) \
         VALUES ($1, $2, $3, $4, $5, '{}'::jsonb, $6, $7)",
    )
    .bind(ledger_id)
    .bind(Uuid::now_v7())
    .bind("POST /transactions")
    .bind("shared-key-001")
    .bind(vec![7_u8; 32])
    .bind(b"{}".to_vec())
    .bind(Utc::now())
    .execute(&db.pool)
    .await
    .expect_err("Duplicate idempotency scope must fail");

    assert_sqlstate(duplicate, "23505");

    post(
        &db.pool,
        ledger_id,
        credit_id,
        debit_id,
        "POST /transactions/one/reverse",
        "reverse-key-001",
        Some(original_id),
    )
    .await;

    let duplicate_reversal = sqlx::query(
        "INSERT INTO ledger.transactions \
         (ledger_id, id, route_scope, idempotency_key, \
          request_hash, response_snapshot, response_body, \
          effective_at, reverses_transaction) \
         VALUES ($1, $2, $3, $4, $5, '{}'::jsonb, \
                 $6, $7, $8)",
    )
    .bind(ledger_id)
    .bind(Uuid::now_v7())
    .bind("POST /transactions/two/reverse")
    .bind("reverse-key-002")
    .bind(vec![7_u8; 32])
    .bind(b"{}".to_vec())
    .bind(Utc::now())
    .bind(original_id)
    .execute(&db.pool)
    .await
    .expect_err("A transaction must not be reversed twice");

    assert_sqlstate(duplicate_reversal, "23505");
}

#[tokio::test]
async fn rejects_cross_ledger_and_timestamp_mismatches() {
    let db = common::TestDb::new().await;

    let (first_ledger, first_account, _) = create_accounts(&db.pool).await;

    let (_, second_account, _) = create_accounts(&db.pool).await;

    for (account_id, timestamp_offset) in [(second_account, 0_i64), (first_account, 1_i64)] {
        let mut transaction = db.pool.begin().await.unwrap();
        let transaction_id = Uuid::now_v7();
        let effective_at = Utc::now();

        insert_transaction(
            &mut transaction,
            first_ledger,
            transaction_id,
            "POST /transactions",
            "foreign-key-test-001",
            effective_at,
            None,
        )
        .await;

        let entry_effective_at = effective_at + chrono::Duration::seconds(timestamp_offset);

        let error = sqlx::query(
            "INSERT INTO ledger.entries \
             (ledger_id, transaction_id, account_id, \
              side, amount, effective_at) \
             VALUES ($1, $2, $3, 'D', 100, $4)",
        )
        .bind(first_ledger)
        .bind(transaction_id)
        .bind(account_id)
        .bind(entry_effective_at)
        .execute(&mut *transaction)
        .await
        .expect_err("Invalid composite foreign key must fail");

        assert_sqlstate(error, "23503");

        // Dropping the aborted transaction rolls it back.
        drop(transaction);
    }
}

#[tokio::test]
async fn rejects_invalid_hashes_and_entry_amounts() {
    let db = common::TestDb::new().await;
    let (ledger_id, account_id, _) = create_accounts(&db.pool).await;

    let invalid_hash = sqlx::query(
        "INSERT INTO ledger.transactions \
         (ledger_id, id, route_scope, idempotency_key, \
          request_hash, response_snapshot, response_body, \
          effective_at) \
         VALUES ($1, $2, $3, $4, $5, '{}'::jsonb, $6, $7)",
    )
    .bind(ledger_id)
    .bind(Uuid::now_v7())
    .bind("POST /transactions")
    .bind("invalid-hash-001")
    .bind(vec![7_u8; 31])
    .bind(b"{}".to_vec())
    .bind(Utc::now())
    .execute(&db.pool)
    .await
    .expect_err("A request hash must be exactly 32 bytes");

    assert_sqlstate(invalid_hash, "23514");

    for amount in [0_i64, 1_000_000_000_001_i64] {
        let mut transaction = db.pool.begin().await.unwrap();
        let transaction_id = Uuid::now_v7();
        let effective_at = Utc::now();

        insert_transaction(
            &mut transaction,
            ledger_id,
            transaction_id,
            "POST /transactions",
            "invalid-amount-001",
            effective_at,
            None,
        )
        .await;

        let error = sqlx::query(
            "INSERT INTO ledger.entries \
             (ledger_id, transaction_id, account_id, \
              side, amount, effective_at) \
             VALUES ($1, $2, $3, 'D', $4, $5)",
        )
        .bind(ledger_id)
        .bind(transaction_id)
        .bind(account_id)
        .bind(amount)
        .bind(effective_at)
        .execute(&mut *transaction)
        .await
        .expect_err("Invalid entry amounts must be rejected");

        assert_sqlstate(error, "23514");
        drop(transaction);
    }
}
