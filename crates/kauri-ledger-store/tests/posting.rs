mod common;

use sqlx::PgPool;
use uuid::Uuid;

type PostResult = (Uuid, bool, Vec<u8>);

fn assert_database_error(error: sqlx::Error, code: &str, marker: &str) {
    match error {
        sqlx::Error::Database(database_error) => {
            assert_eq!(database_error.code().as_deref(), Some(code));

            if !marker.is_empty() {
                assert!(
                    database_error.message().contains(marker),
                    "Expected {marker}, got: {}",
                    database_error.message()
                );
            }
        }
        other => panic!("Expected a database error, got: {other:?}"),
    }
}

fn entries_json(entries: &[(Uuid, &str, i64)]) -> String {
    let values: Vec<String> = entries
        .iter()
        .map(|(account_id, side, amount)| {
            format!(r#"{{"account_id":"{account_id}","side":"{side}","amount":{amount}}}"#)
        })
        .collect();

    format!("[{}]", values.join(","))
}

async fn setup_accounts(pool: &PgPool) -> (Uuid, Uuid, Uuid, Uuid) {
    let ledger_id = Uuid::now_v7();
    let funding_id = Uuid::now_v7();
    let wallet_id = Uuid::now_v7();
    let receiver_id = Uuid::now_v7();

    sqlx::query("INSERT INTO ledger.ledgers (id, name) VALUES ($1, $2)")
        .bind(ledger_id)
        .bind("Posting Test Ledger")
        .execute(pool)
        .await
        .unwrap();

    for (account_id, code, allow_negative) in [
        (funding_id, "asset:funding", true),
        (wallet_id, "asset:wallet", false),
        (receiver_id, "asset:receiver", false),
    ] {
        let created: Uuid = sqlx::query_scalar(
            "SELECT ledger.create_account(\
             $1, $2, $3, 'asset', 'Test Account', $4)",
        )
        .bind(ledger_id)
        .bind(account_id)
        .bind(code)
        .bind(allow_negative)
        .fetch_one(pool)
        .await
        .unwrap();

        assert_eq!(created, account_id);
    }

    (ledger_id, funding_id, wallet_id, receiver_id)
}

async fn post_raw(
    pool: &PgPool,
    ledger_id: Uuid,
    transaction_id: Uuid,
    key: &str,
    hash_byte: u8,
    response: &[u8],
    entries: &str,
) -> Result<PostResult, sqlx::Error> {
    sqlx::query_as(
        "SELECT posted_transaction_id, replayed, stored_response_body \
         FROM ledger.post_transaction( \
             $1, $2, $3, $4, $5, $6, $7, $8::jsonb \
         )",
    )
    .bind(ledger_id)
    .bind(transaction_id)
    .bind("POST /transactions")
    .bind(key)
    .bind(vec![hash_byte; 32])
    .bind(response.to_vec())
    .bind(chrono::Utc::now())
    .bind(entries)
    .fetch_one(pool)
    .await
}

async fn fund_wallet(pool: &PgPool, ledger_id: Uuid, funding_id: Uuid, wallet_id: Uuid) {
    let entries = entries_json(&[(wallet_id, "D", 100), (funding_id, "C", 100)]);

    let result = post_raw(
        pool,
        ledger_id,
        Uuid::now_v7(),
        "fund-wallet-001",
        1,
        br#"{"status":"funded"}"#,
        &entries,
    )
    .await
    .unwrap();

    assert!(!result.1);
}

async fn wallet_balance(pool: &PgPool, ledger_id: Uuid, wallet_id: Uuid) -> (i64, i64, i64) {
    sqlx::query_as(
        "SELECT debit_total, credit_total, version \
         FROM ledger.account_balances \
         WHERE ledger_id = $1 AND account_id = $2",
    )
    .bind(ledger_id)
    .bind(wallet_id)
    .fetch_one(pool)
    .await
    .unwrap()
}

#[tokio::test]
async fn posts_balanced_entries_and_replays_original_bytes() {
    let db = common::TestDb::new().await;
    let (ledger_id, funding_id, wallet_id, _) = setup_accounts(&db.pool).await;

    let entries = entries_json(&[(wallet_id, "D", 100), (funding_id, "C", 100)]);

    let original_body = br#"{"status":"accepted"}"#;

    let first = post_raw(
        &db.pool,
        ledger_id,
        Uuid::now_v7(),
        "original-post-001",
        5,
        original_body,
        &entries,
    )
    .await
    .unwrap();

    assert!(!first.1);
    assert_eq!(first.2, original_body);

    assert_eq!(
        wallet_balance(&db.pool, ledger_id, wallet_id).await,
        (100, 0, 1)
    );

    let sealed_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM ledger.transaction_seals \
         WHERE ledger_id = $1",
    )
    .bind(ledger_id)
    .fetch_one(&db.pool)
    .await
    .unwrap();

    assert_eq!(sealed_count, 1);

    // A later freeze must not prevent replay of a committed request.
    sqlx::query(
        "UPDATE ledger.ledgers SET status = 'frozen' \
         WHERE id = $1",
    )
    .bind(ledger_id)
    .execute(&db.pool)
    .await
    .unwrap();

    let replay = post_raw(
        &db.pool,
        ledger_id,
        Uuid::now_v7(),
        "original-post-001",
        5,
        br#"{"status":"different"}"#,
        &entries,
    )
    .await
    .unwrap();

    assert_eq!(replay.0, first.0);
    assert!(replay.1);
    assert_eq!(replay.2, original_body);

    assert_eq!(
        wallet_balance(&db.pool, ledger_id, wallet_id).await,
        (100, 0, 1)
    );
}

#[tokio::test]
async fn rejects_reused_idempotency_keys_with_different_hashes() {
    let db = common::TestDb::new().await;
    let (ledger_id, funding_id, wallet_id, _) = setup_accounts(&db.pool).await;

    let entries = entries_json(&[(wallet_id, "D", 100), (funding_id, "C", 100)]);

    post_raw(
        &db.pool,
        ledger_id,
        Uuid::now_v7(),
        "idempotency-key-001",
        1,
        b"{}",
        &entries,
    )
    .await
    .unwrap();

    let error = post_raw(
        &db.pool,
        ledger_id,
        Uuid::now_v7(),
        "idempotency-key-001",
        2,
        b"{}",
        &entries,
    )
    .await
    .expect_err("Reused keys with different hashes must fail");

    assert_database_error(error, "23514", "IDEMPOTENCY_KEY_REUSED");

    assert_eq!(
        wallet_balance(&db.pool, ledger_id, wallet_id).await,
        (100, 0, 1)
    );
}

#[tokio::test]
async fn rejects_invalid_and_unbalanced_entry_batches() {
    let db = common::TestDb::new().await;
    let (ledger_id, funding_id, wallet_id, _) = setup_accounts(&db.pool).await;

    let single = entries_json(&[(wallet_id, "D", 10)]);
    let unbalanced = entries_json(&[(wallet_id, "D", 10), (funding_id, "C", 9)]);

    let too_many: Vec<_> = (0..33)
        .flat_map(|_| [(wallet_id, "D", 1), (funding_id, "C", 1)])
        .collect();

    let cases = [
        ("invalid-empty-001", "[]".to_string()),
        ("invalid-single-001", single),
        ("invalid-unbalanced-001", unbalanced),
        ("invalid-overflow-001", entries_json(&too_many)),
    ];

    for (key, entries) in cases {
        let error = post_raw(&db.pool, ledger_id, Uuid::now_v7(), key, 4, b"{}", &entries)
            .await
            .expect_err("Invalid entry batches must be rejected");

        assert_database_error(error, "23514", "");
    }

    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM ledger.transactions \
         WHERE ledger_id = $1",
    )
    .bind(ledger_id)
    .fetch_one(&db.pool)
    .await
    .unwrap();

    assert_eq!(count, 0);
}

#[tokio::test]
async fn insufficient_funds_roll_back_the_entire_posting() {
    let db = common::TestDb::new().await;
    let (ledger_id, funding_id, wallet_id, receiver_id) = setup_accounts(&db.pool).await;

    fund_wallet(&db.pool, ledger_id, funding_id, wallet_id).await;

    let spending = entries_json(&[(receiver_id, "D", 150), (wallet_id, "C", 150)]);

    let error = post_raw(
        &db.pool,
        ledger_id,
        Uuid::now_v7(),
        "overdraft-attempt-001",
        6,
        b"{}",
        &spending,
    )
    .await
    .expect_err("A wallet must not spend more than its balance");

    assert_database_error(error, "23514", "");

    assert_eq!(
        wallet_balance(&db.pool, ledger_id, wallet_id).await,
        (100, 0, 1)
    );

    let (transaction_count, entry_count): (i64, i64) = sqlx::query_as(
        "SELECT \
             (SELECT COUNT(*) FROM ledger.transactions \
              WHERE ledger_id = $1), \
             (SELECT COUNT(*) FROM ledger.entries \
              WHERE ledger_id = $1)",
    )
    .bind(ledger_id)
    .fetch_one(&db.pool)
    .await
    .unwrap();

    assert_eq!((transaction_count, entry_count), (1, 2));
}

#[tokio::test]
async fn concurrent_spending_cannot_overdraw_an_account() {
    let db = common::TestDb::new().await;
    let (ledger_id, funding_id, wallet_id, receiver_id) = setup_accounts(&db.pool).await;

    fund_wallet(&db.pool, ledger_id, funding_id, wallet_id).await;

    let spending = entries_json(&[(receiver_id, "D", 80), (wallet_id, "C", 80)]);

    let first = post_raw(
        &db.pool,
        ledger_id,
        Uuid::now_v7(),
        "concurrent-spend-001",
        7,
        b"{}",
        &spending,
    );

    let second = post_raw(
        &db.pool,
        ledger_id,
        Uuid::now_v7(),
        "concurrent-spend-002",
        8,
        b"{}",
        &spending,
    );

    let (first_result, second_result) = tokio::join!(first, second);

    let successful = usize::from(first_result.is_ok()) + usize::from(second_result.is_ok());

    assert_eq!(
        successful, 1,
        "Exactly one of the two spending attempts must succeed"
    );

    for result in [first_result, second_result] {
        if let Err(error) = result {
            assert_database_error(error, "23514", "");
        }
    }

    assert_eq!(
        wallet_balance(&db.pool, ledger_id, wallet_id).await,
        (100, 80, 2)
    );

    let transaction_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM ledger.transactions \
         WHERE ledger_id = $1",
    )
    .bind(ledger_id)
    .fetch_one(&db.pool)
    .await
    .unwrap();

    assert_eq!(transaction_count, 2);
}

#[tokio::test]
async fn concurrent_retries_create_only_one_transaction() {
    let db = common::TestDb::new().await;
    let (ledger_id, funding_id, wallet_id, _) = setup_accounts(&db.pool).await;

    let entries = entries_json(&[(wallet_id, "D", 100), (funding_id, "C", 100)]);

    let first = post_raw(
        &db.pool,
        ledger_id,
        Uuid::now_v7(),
        "concurrent-replay-001",
        9,
        br#"{"source":"first"}"#,
        &entries,
    );

    let second = post_raw(
        &db.pool,
        ledger_id,
        Uuid::now_v7(),
        "concurrent-replay-001",
        9,
        br#"{"source":"second"}"#,
        &entries,
    );

    let (first_result, second_result) = tokio::join!(first, second);

    let first_result = first_result.unwrap();
    let second_result = second_result.unwrap();

    assert_eq!(first_result.0, second_result.0);
    assert_eq!(first_result.2, second_result.2);
    assert_ne!(first_result.1, second_result.1);

    assert_eq!(
        wallet_balance(&db.pool, ledger_id, wallet_id).await,
        (100, 0, 1)
    );

    let transaction_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM ledger.transactions \
         WHERE ledger_id = $1",
    )
    .bind(ledger_id)
    .fetch_one(&db.pool)
    .await
    .unwrap();

    assert_eq!(transaction_count, 1);
}

#[tokio::test]
async fn rejects_closed_accounts_and_cross_ledger_entries() {
    let db = common::TestDb::new().await;
    let (ledger_id, funding_id, wallet_id, _) = setup_accounts(&db.pool).await;

    let (_, foreign_id, _, _) = setup_accounts(&db.pool).await;

    sqlx::query(
        "UPDATE ledger.accounts \
         SET status = 'closed' \
         WHERE ledger_id = $1 AND id = $2",
    )
    .bind(ledger_id)
    .bind(wallet_id)
    .execute(&db.pool)
    .await
    .unwrap();

    let entries = entries_json(&[(wallet_id, "D", 10), (funding_id, "C", 10)]);

    let error = post_raw(
        &db.pool,
        ledger_id,
        Uuid::now_v7(),
        "closed-account-001",
        10,
        b"{}",
        &entries,
    )
    .await
    .expect_err("Closed accounts must not accept new entries");

    assert_database_error(error, "23514", "ACCOUNT_CLOSED");

    sqlx::query(
        "UPDATE ledger.accounts \
         SET status = 'active' \
         WHERE ledger_id = $1 AND id = $2",
    )
    .bind(ledger_id)
    .bind(wallet_id)
    .execute(&db.pool)
    .await
    .unwrap();

    let cross_ledger = entries_json(&[(wallet_id, "D", 10), (foreign_id, "C", 10)]);

    let error = post_raw(
        &db.pool,
        ledger_id,
        Uuid::now_v7(),
        "cross-ledger-001",
        11,
        b"{}",
        &cross_ledger,
    )
    .await
    .expect_err("Cross-ledger account references must be rejected");

    assert_database_error(error, "23503", "");

    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM ledger.transactions \
         WHERE ledger_id = $1",
    )
    .bind(ledger_id)
    .fetch_one(&db.pool)
    .await
    .unwrap();

    assert_eq!(count, 0);
}

#[tokio::test]
async fn application_role_cannot_execute_posting_function() {
    let db = common::TestDb::new().await;
    let (ledger_id, funding_id, wallet_id, _) = setup_accounts(&db.pool).await;

    let entries = entries_json(&[(wallet_id, "D", 10), (funding_id, "C", 10)]);

    let mut transaction = db.pool.begin().await.unwrap();

    sqlx::query("SET LOCAL ROLE ledger_app")
        .execute(&mut *transaction)
        .await
        .unwrap();

    let error = sqlx::query_as::<_, PostResult>(
        "SELECT posted_transaction_id, replayed, stored_response_body \
         FROM ledger.post_transaction( \
             $1, $2, $3, $4, $5, $6, $7, $8::jsonb \
         )",
    )
    .bind(ledger_id)
    .bind(Uuid::now_v7())
    .bind("POST /transactions")
    .bind("unauthorized-post-001")
    .bind(vec![12_u8; 32])
    .bind(b"{}".to_vec())
    .bind(chrono::Utc::now())
    .bind(entries)
    .fetch_one(&mut *transaction)
    .await
    .expect_err("Application role must not execute privileged posting");

    assert_database_error(error, "42501", "");

    transaction.rollback().await.unwrap();
}
