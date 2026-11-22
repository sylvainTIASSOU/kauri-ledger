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

#[tokio::test]
async fn rejects_transactions_without_enough_entries_at_commit() {
    let db = common::TestDb::new().await;
    let (ledger_id, debit_id, _) = create_accounts(&db.pool).await;

    for (entry_count, key) in [
        (0_usize, "empty-transaction-001"),
        (1_usize, "single-entry-001"),
    ] {
        let mut transaction = db.pool.begin().await.unwrap();
        let transaction_id = Uuid::now_v7();
        let effective_at = Utc::now();

        insert_transaction(
            &mut transaction,
            ledger_id,
            transaction_id,
            "POST /transactions",
            key,
            effective_at,
            None,
        )
        .await;

        if entry_count == 1 {
            sqlx::query(
                "INSERT INTO ledger.entries \
                 (ledger_id, transaction_id, account_id, \
                  side, amount, effective_at) \
                 VALUES ($1, $2, $3, 'D', 100, $4)",
            )
            .bind(ledger_id)
            .bind(transaction_id)
            .bind(debit_id)
            .bind(effective_at)
            .execute(&mut *transaction)
            .await
            .unwrap();
        }

        let error = transaction
            .commit()
            .await
            .expect_err("Incomplete transactions must not commit");

        assert_sqlstate(error, "23514");
    }
}

#[tokio::test]
async fn rejects_unbalanced_transactions_at_commit() {
    let db = common::TestDb::new().await;
    let (ledger_id, debit_id, credit_id) = create_accounts(&db.pool).await;

    let mut transaction = db.pool.begin().await.unwrap();
    let transaction_id = Uuid::now_v7();
    let effective_at = Utc::now();

    insert_transaction(
        &mut transaction,
        ledger_id,
        transaction_id,
        "POST /transactions",
        "unbalanced-transaction-001",
        effective_at,
        None,
    )
    .await;

    for (account_id, side, amount) in [(debit_id, "D", 100_i64), (credit_id, "C", 99_i64)] {
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
        .bind(amount)
        .bind(effective_at)
        .execute(&mut *transaction)
        .await
        .unwrap();
    }

    let error = transaction
        .commit()
        .await
        .expect_err("Unbalanced transactions must not commit");

    assert_sqlstate(error, "23514");
}

#[tokio::test]
async fn rejects_transactions_with_more_than_64_entries() {
    let db = common::TestDb::new().await;
    let (ledger_id, debit_id, credit_id) = create_accounts(&db.pool).await;

    let mut transaction = db.pool.begin().await.unwrap();
    let transaction_id = Uuid::now_v7();
    let effective_at = Utc::now();

    insert_transaction(
        &mut transaction,
        ledger_id,
        transaction_id,
        "POST /transactions",
        "too-many-entries-001",
        effective_at,
        None,
    )
    .await;

    // 33 debit entries and 33 credit entries.
    // The transaction is balanced but exceeds the entry limit.
    for _ in 0..33 {
        for (account_id, side) in [(debit_id, "D"), (credit_id, "C")] {
            sqlx::query(
                "INSERT INTO ledger.entries \
                 (ledger_id, transaction_id, account_id, \
                  side, amount, effective_at) \
                 VALUES ($1, $2, $3, $4, 1, $5)",
            )
            .bind(ledger_id)
            .bind(transaction_id)
            .bind(account_id)
            .bind(side)
            .bind(effective_at)
            .execute(&mut *transaction)
            .await
            .unwrap();
        }
    }

    let error = transaction
        .commit()
        .await
        .expect_err("Transactions with 66 entries must not commit");

    assert_sqlstate(error, "23514");
}

#[tokio::test]
async fn seals_committed_transactions_against_late_entries() {
    let db = common::TestDb::new().await;
    let (ledger_id, debit_id, credit_id) = create_accounts(&db.pool).await;

    let transaction_id = post(
        &db.pool,
        ledger_id,
        debit_id,
        credit_id,
        "POST /transactions",
        "sealed-transaction-001",
        None,
    )
    .await;

    let seal_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM ledger.transaction_seals \
         WHERE ledger_id = $1 AND transaction_id = $2",
    )
    .bind(ledger_id)
    .bind(transaction_id)
    .fetch_one(&db.pool)
    .await
    .unwrap();

    assert_eq!(seal_count, 1);

    let effective_at: chrono::DateTime<Utc> = sqlx::query_scalar(
        "SELECT effective_at FROM ledger.transactions \
             WHERE ledger_id = $1 AND id = $2",
    )
    .bind(ledger_id)
    .bind(transaction_id)
    .fetch_one(&db.pool)
    .await
    .unwrap();

    let error = sqlx::query(
        "INSERT INTO ledger.entries \
         (ledger_id, transaction_id, account_id, \
          side, amount, effective_at) \
         VALUES ($1, $2, $3, 'D', 100, $4)",
    )
    .bind(ledger_id)
    .bind(transaction_id)
    .bind(debit_id)
    .bind(effective_at)
    .execute(&db.pool)
    .await
    .expect_err("Sealed transactions must reject late entries");

    assert_sqlstate(error, "23514");
}

#[tokio::test]
async fn rejects_mutation_and_truncation_of_posted_data() {
    let db = common::TestDb::new().await;
    let (ledger_id, debit_id, credit_id) = create_accounts(&db.pool).await;

    post(
        &db.pool,
        ledger_id,
        debit_id,
        credit_id,
        "POST /transactions",
        "immutable-transaction-001",
        None,
    )
    .await;

    let invalid_statements = [
        "UPDATE ledger.transactions SET reference = 'tampered'",
        "DELETE FROM ledger.transactions",
        "UPDATE ledger.entries SET amount = 999",
        "DELETE FROM ledger.entries",
        "DELETE FROM ledger.transaction_seals",
        "TRUNCATE ledger.entries",
        "TRUNCATE ledger.transaction_seals",
    ];

    for statement in invalid_statements {
        let error = sqlx::query(statement)
            .execute(&db.pool)
            .await
            .expect_err("Append-only tables must reject mutation");

        assert_sqlstate(error, "23514");
    }
}

fn current_second() -> DateTime<Utc> {
    DateTime::<Utc>::from_timestamp(Utc::now().timestamp(), 0)
        .expect("Current timestamp must be valid")
}

fn assert_rule_violation(error: sqlx::Error, marker: &str) {
    match &error {
        sqlx::Error::Database(database_error) => {
            assert!(
                database_error.message().contains(marker),
                "Expected error marker {marker}, got: {}",
                database_error.message()
            );
        }
        other => panic!("Expected a PostgreSQL error, got: {other:?}"),
    }

    assert_sqlstate(error, "23514");
}

async fn attempt_new_transaction(
    pool: &PgPool,
    ledger_id: Uuid,
    key: &str,
    effective_at: DateTime<Utc>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO ledger.transactions \
         (ledger_id, id, route_scope, idempotency_key, \
          request_hash, response_snapshot, response_body, \
          effective_at) \
         VALUES ($1, $2, $3, $4, $5, '{}'::jsonb, $6, $7)",
    )
    .bind(ledger_id)
    .bind(Uuid::now_v7())
    .bind("POST /transactions")
    .bind(key)
    .bind(vec![7_u8; 32])
    .bind(b"{}".to_vec())
    .bind(effective_at)
    .execute(pool)
    .await
    .map(|_| ())
}

async fn close_period(pool: &PgPool, ledger_id: Uuid, cutoff: DateTime<Utc>) {
    sqlx::query("SELECT ledger.close_period($1, $2)")
        .bind(ledger_id)
        .bind(cutoff)
        .execute(pool)
        .await
        .unwrap();
}

#[tokio::test]
async fn frozen_ledger_rejects_new_transactions() {
    let db = common::TestDb::new().await;
    let (ledger_id, debit_id, credit_id) = create_accounts(&db.pool).await;

    sqlx::query(
        "UPDATE ledger.ledgers \
         SET status = 'frozen' WHERE id = $1",
    )
    .bind(ledger_id)
    .execute(&db.pool)
    .await
    .unwrap();

    let error = attempt_new_transaction(&db.pool, ledger_id, "frozen-ledger-key-001", Utc::now())
        .await
        .expect_err("Frozen ledgers must reject postings");

    assert_rule_violation(error, "LEDGER_FROZEN");

    // Administrative reactivation allows new postings.
    sqlx::query(
        "UPDATE ledger.ledgers \
         SET status = 'active' WHERE id = $1",
    )
    .bind(ledger_id)
    .execute(&db.pool)
    .await
    .unwrap();

    post(
        &db.pool,
        ledger_id,
        debit_id,
        credit_id,
        "POST /transactions",
        "reactivated-ledger-001",
        None,
    )
    .await;
}

#[tokio::test]
async fn closed_period_rejects_backdated_and_boundary_postings() {
    let db = common::TestDb::new().await;
    let (ledger_id, debit_id, credit_id) = create_accounts(&db.pool).await;

    let cutoff = current_second();
    close_period(&db.pool, ledger_id, cutoff).await;

    for (key, effective_at) in [
        (
            "backdated-posting-001",
            cutoff - chrono::Duration::seconds(1),
        ),
        ("boundary-posting-001", cutoff),
    ] {
        let error = attempt_new_transaction(&db.pool, ledger_id, key, effective_at)
            .await
            .expect_err("Closed periods must reject postings");

        assert_rule_violation(error, "PERIOD_LOCKED");
    }

    // A new transaction after the cutoff remains valid.
    post(
        &db.pool,
        ledger_id,
        debit_id,
        credit_id,
        "POST /transactions",
        "post-closure-key-001",
        None,
    )
    .await;
}

#[tokio::test]
async fn period_closure_cannot_move_backward() {
    let db = common::TestDb::new().await;
    let (ledger_id, _, _) = create_accounts(&db.pool).await;

    let cutoff = current_second();

    close_period(&db.pool, ledger_id, cutoff).await;

    let error = sqlx::query("SELECT ledger.close_period($1, $2)")
        .bind(ledger_id)
        .bind(cutoff - chrono::Duration::seconds(1))
        .execute(&db.pool)
        .await
        .expect_err("Period closures must be monotonic");

    assert_rule_violation(error, "PERIOD_LOCK_REWIND");

    // Repeating the same cutoff is idempotent.
    close_period(&db.pool, ledger_id, cutoff).await;

    let actual_cutoff: DateTime<Utc> = sqlx::query_scalar(
        "SELECT locked_until FROM ledger.period_locks \
         WHERE ledger_id = $1",
    )
    .bind(ledger_id)
    .fetch_one(&db.pool)
    .await
    .unwrap();

    assert_eq!(actual_cutoff, cutoff);
}

#[tokio::test]
async fn posting_prevents_concurrent_freeze_from_overtaking() {
    let db = common::TestDb::new().await;
    let (ledger_id, _, _) = create_accounts(&db.pool).await;

    let mut posting = db.pool.begin().await.unwrap();

    insert_transaction(
        &mut posting,
        ledger_id,
        Uuid::now_v7(),
        "POST /transactions",
        "inflight-freeze-test-001",
        Utc::now(),
        None,
    )
    .await;

    let mut administrative = db.pool.begin().await.unwrap();

    sqlx::query("SET LOCAL lock_timeout = '150ms'")
        .execute(&mut *administrative)
        .await
        .unwrap();

    let error = sqlx::query(
        "UPDATE ledger.ledgers \
         SET status = 'frozen' WHERE id = $1",
    )
    .bind(ledger_id)
    .execute(&mut *administrative)
    .await
    .expect_err("Freeze must wait for the in-flight posting");

    assert_sqlstate(error, "55P03");

    administrative.rollback().await.unwrap();
    posting.rollback().await.unwrap();

    // After the posting ends, freezing succeeds.
    sqlx::query(
        "UPDATE ledger.ledgers \
         SET status = 'frozen' WHERE id = $1",
    )
    .bind(ledger_id)
    .execute(&db.pool)
    .await
    .unwrap();

    let status: String = sqlx::query_scalar("SELECT status FROM ledger.ledgers WHERE id = $1")
        .bind(ledger_id)
        .fetch_one(&db.pool)
        .await
        .unwrap();

    assert_eq!(status, "frozen");
}

#[tokio::test]
async fn posting_prevents_concurrent_period_closure() {
    let db = common::TestDb::new().await;
    let (ledger_id, _, _) = create_accounts(&db.pool).await;

    let mut posting = db.pool.begin().await.unwrap();

    insert_transaction(
        &mut posting,
        ledger_id,
        Uuid::now_v7(),
        "POST /transactions",
        "inflight-period-test-001",
        Utc::now(),
        None,
    )
    .await;

    let mut administrative = db.pool.begin().await.unwrap();

    sqlx::query("SET LOCAL lock_timeout = '150ms'")
        .execute(&mut *administrative)
        .await
        .unwrap();

    let cutoff = current_second();

    let error = sqlx::query("SELECT ledger.close_period($1, $2)")
        .bind(ledger_id)
        .bind(cutoff)
        .execute(&mut *administrative)
        .await
        .expect_err("Period closure must wait for the posting");

    assert_sqlstate(error, "55P03");

    administrative.rollback().await.unwrap();
    posting.rollback().await.unwrap();

    // The period can be closed after the posting ends.
    close_period(&db.pool, ledger_id, cutoff).await;

    let actual_cutoff: DateTime<Utc> = sqlx::query_scalar(
        "SELECT locked_until FROM ledger.period_locks \
         WHERE ledger_id = $1",
    )
    .bind(ledger_id)
    .fetch_one(&db.pool)
    .await
    .unwrap();

    assert_eq!(actual_cutoff, cutoff);
}
