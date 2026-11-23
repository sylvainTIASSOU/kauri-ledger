mod common;

use chrono::Utc;
use kauri_ledger_core::{AccountCode, AccountType, Amount, Direction};
use kauri_ledger_store::idempotency::compute_request_hash;
use kauri_ledger_store::{
    StoreError,
    tenant_repository::{
        TenantLedgerEntry, TenantPosting, TenantPostingResult, create_ledger_account,
        post_tenant_transaction,
    },
};
use sqlx::PgPool;
use uuid::Uuid;

fn entry(account_id: Uuid, direction: Direction, amount: i64) -> TenantLedgerEntry {
    TenantLedgerEntry {
        account_id,
        direction,
        amount: Amount::new(amount).unwrap(),
    }
}

fn assert_sqlstate(error: StoreError, expected: &str) {
    match error {
        StoreError::Database(sqlx::Error::Database(database_error)) => {
            assert_eq!(database_error.code().as_deref(), Some(expected),);
        }
        other => panic!("Expected PostgreSQL error, got: {other:?}"),
    }
}

async fn setup(pool: &PgPool) -> (Uuid, Uuid, Uuid, Uuid) {
    let ledger_id = Uuid::now_v7();
    let funding_id = Uuid::now_v7();
    let wallet_id = Uuid::now_v7();
    let receiver_id = Uuid::now_v7();

    sqlx::query(
        "INSERT INTO ledger.ledgers (id, name) \
         VALUES ($1, $2)",
    )
    .bind(ledger_id)
    .bind("Rust Repository Test Ledger")
    .execute(pool)
    .await
    .unwrap();

    for (id, code, allow_negative) in [
        (funding_id, "asset:funding", true),
        (wallet_id, "asset:wallet", false),
        (receiver_id, "asset:receiver", false),
    ] {
        let code = AccountCode::new(code).unwrap();

        let created = create_ledger_account(
            pool,
            ledger_id,
            id,
            &code,
            AccountType::Asset,
            "Test Account",
            allow_negative,
        )
        .await
        .unwrap();

        assert_eq!(created, id);
    }

    (ledger_id, funding_id, wallet_id, receiver_id)
}

async fn post(
    pool: &PgPool,
    ledger_id: Uuid,
    key: &str,
    hash_byte: u8,
    response: &[u8],
    entries: &[TenantLedgerEntry],
) -> Result<TenantPostingResult, StoreError> {
    let raw_request_body = format!(r#"{{"test_case":{hash_byte}}}"#);

    let route_scope = format!("POST /v1/ledgers/{ledger_id}/transactions");

    let request = TenantPosting {
        ledger_id,
        transaction_id: Uuid::now_v7(),
        route_scope: &route_scope,
        idempotency_key: key,
        raw_request_body: raw_request_body.as_bytes(),
        response_body: response,
        effective_at: Utc::now(),
        entries,
    };

    post_tenant_transaction(pool, &request).await
}

async fn wallet_totals(pool: &PgPool, ledger_id: Uuid, wallet_id: Uuid) -> (i64, i64, i64) {
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

async fn fund_wallet(pool: &PgPool, ledger_id: Uuid, funding_id: Uuid, wallet_id: Uuid) {
    let entries = [
        entry(wallet_id, Direction::Debit, 100),
        entry(funding_id, Direction::Credit, 100),
    ];

    post(
        pool,
        ledger_id,
        "rust-funding-001",
        1,
        br#"{"status":"funded"}"#,
        &entries,
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn creates_account_with_matching_initial_balance() {
    let db = common::TestDb::new().await;
    let (ledger_id, _, wallet_id, _) = setup(&db.pool).await;

    let result: (String, i64, i64, i64) = sqlx::query_as(
        "SELECT b.normal_side::text, \
                b.debit_total, b.credit_total, b.version \
         FROM ledger.account_balances AS b \
         WHERE b.ledger_id = $1 AND b.account_id = $2",
    )
    .bind(ledger_id)
    .bind(wallet_id)
    .fetch_one(&db.pool)
    .await
    .unwrap();

    assert_eq!(result, ("D".to_owned(), 0, 0, 0));

    let liability_id = Uuid::now_v7();
    let liability_code = AccountCode::new("liability:debt").unwrap();

    create_ledger_account(
        &db.pool,
        ledger_id,
        liability_id,
        &liability_code,
        AccountType::Liability,
        "Debt Account",
        false,
    )
    .await
    .unwrap();

    let normal_side: String = sqlx::query_scalar(
        "SELECT normal_side::text \
         FROM ledger.account_balances \
         WHERE ledger_id = $1 AND account_id = $2",
    )
    .bind(ledger_id)
    .bind(liability_id)
    .fetch_one(&db.pool)
    .await
    .unwrap();

    assert_eq!(normal_side, "C");
}

#[tokio::test]
async fn posts_entries_and_replays_the_original_response() {
    let db = common::TestDb::new().await;
    let (ledger_id, funding_id, wallet_id, _) = setup(&db.pool).await;

    let entries = [
        entry(wallet_id, Direction::Debit, 100),
        entry(funding_id, Direction::Credit, 100),
    ];

    let original_body = br#"{"status":"posted"}"#;

    let first = post(
        &db.pool,
        ledger_id,
        "rust-posting-001",
        2,
        original_body,
        &entries,
    )
    .await
    .unwrap();

    assert!(!first.replayed);
    assert_eq!(first.response_body.as_slice(), original_body);

    let replay = post(
        &db.pool,
        ledger_id,
        "rust-posting-001",
        2,
        br#"{"status":"different"}"#,
        &entries,
    )
    .await
    .unwrap();

    assert!(replay.replayed);
    assert_eq!(replay.transaction_id, first.transaction_id);
    assert_eq!(replay.response_body, first.response_body);

    assert_eq!(
        wallet_totals(&db.pool, ledger_id, wallet_id).await,
        (100, 0, 1)
    );

    let sides: Vec<String> = sqlx::query_scalar(
        "SELECT side::text FROM ledger.entries \
         WHERE ledger_id = $1 AND transaction_id = $2 \
         ORDER BY id",
    )
    .bind(ledger_id)
    .bind(first.transaction_id)
    .fetch_all(&db.pool)
    .await
    .unwrap();

    assert_eq!(sides, ["D", "C"]);
}

#[tokio::test]
async fn rejected_posting_does_not_change_balances() {
    let db = common::TestDb::new().await;
    let (ledger_id, funding_id, wallet_id, receiver_id) = setup(&db.pool).await;

    fund_wallet(&db.pool, ledger_id, funding_id, wallet_id).await;

    let spending = [
        entry(receiver_id, Direction::Debit, 150),
        entry(wallet_id, Direction::Credit, 150),
    ];

    let error = post(
        &db.pool,
        ledger_id,
        "rust-overdraft-001",
        3,
        b"{}",
        &spending,
    )
    .await
    .expect_err("Insufficient funds must reject posting");

    assert_sqlstate(error, "23514");

    assert_eq!(
        wallet_totals(&db.pool, ledger_id, wallet_id).await,
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
async fn concurrent_rust_postings_cannot_overdraw() {
    let db = common::TestDb::new().await;
    let (ledger_id, funding_id, wallet_id, receiver_id) = setup(&db.pool).await;

    fund_wallet(&db.pool, ledger_id, funding_id, wallet_id).await;

    let spending = [
        entry(receiver_id, Direction::Debit, 80),
        entry(wallet_id, Direction::Credit, 80),
    ];

    let first = post(
        &db.pool,
        ledger_id,
        "rust-spending-001",
        4,
        b"{}",
        &spending,
    );

    let second = post(
        &db.pool,
        ledger_id,
        "rust-spending-002",
        5,
        b"{}",
        &spending,
    );

    let (first_result, second_result) = tokio::join!(first, second);

    let successes = usize::from(first_result.is_ok()) + usize::from(second_result.is_ok());

    assert_eq!(successes, 1);

    for result in [first_result, second_result] {
        if let Err(error) = result {
            assert_sqlstate(error, "23514");
        }
    }

    assert_eq!(
        wallet_totals(&db.pool, ledger_id, wallet_id).await,
        (100, 80, 2)
    );
}

async fn post_with_raw_body(
    pool: &PgPool,
    ledger_id: Uuid,
    key: &str,
    raw_body: &[u8],
    response: &[u8],
    entries: &[TenantLedgerEntry],
) -> Result<TenantPostingResult, StoreError> {
    let route_scope = format!("POST /v1/ledgers/{ledger_id}/transactions");

    let request = TenantPosting {
        ledger_id,
        transaction_id: Uuid::now_v7(),
        route_scope: &route_scope,
        idempotency_key: key,
        raw_request_body: raw_body,
        response_body: response,
        effective_at: Utc::now(),
        entries,
    };

    post_tenant_transaction(pool, &request).await
}

#[tokio::test]
async fn canonical_json_replays_and_persists_the_expected_hash() {
    let db = common::TestDb::new().await;
    let (ledger_id, funding_id, wallet_id, _) = setup(&db.pool).await;

    let entries = [
        entry(wallet_id, Direction::Debit, 100),
        entry(funding_id, Direction::Credit, 100),
    ];

    let original_body = format!(
        r#"{{"from":"{funding_id}","to":"{wallet_id}","amount":100,"metadata":{{"b":2,"a":1}}}}"#
    );

    let equivalent_body = format!(
        r#"{{"metadata":{{"a":1,"b":2}},"amount":100,"to":"{wallet_id}","from":"{funding_id}"}}"#
    );

    let first = post_with_raw_body(
        &db.pool,
        ledger_id,
        "canonical-replay-001",
        original_body.as_bytes(),
        br#"{"status":"accepted"}"#,
        &entries,
    )
    .await
    .unwrap();

    assert!(!first.replayed);

    let replay = post_with_raw_body(
        &db.pool,
        ledger_id,
        "canonical-replay-001",
        equivalent_body.as_bytes(),
        br#"{"status":"changed"}"#,
        &entries,
    )
    .await
    .unwrap();

    assert!(replay.replayed);
    assert_eq!(replay.transaction_id, first.transaction_id);
    assert_eq!(replay.response_body, first.response_body);

    let stored_hash: Vec<u8> = sqlx::query_scalar(
        "SELECT request_hash FROM ledger.transactions \
         WHERE ledger_id = $1 AND id = $2",
    )
    .bind(ledger_id)
    .bind(first.transaction_id)
    .fetch_one(&db.pool)
    .await
    .unwrap();

    let request_path = format!("/v1/ledgers/{ledger_id}/transactions");

    let expected_hash =
        compute_request_hash("POST", &request_path, original_body.as_bytes()).unwrap();

    assert_eq!(stored_hash, expected_hash.to_vec());

    assert_eq!(
        wallet_totals(&db.pool, ledger_id, wallet_id).await,
        (100, 0, 1)
    );
}

#[tokio::test]
async fn different_json_with_the_same_key_is_rejected() {
    let db = common::TestDb::new().await;
    let (ledger_id, funding_id, wallet_id, _) = setup(&db.pool).await;

    let entries = [
        entry(wallet_id, Direction::Debit, 20),
        entry(funding_id, Direction::Credit, 20),
    ];

    post_with_raw_body(
        &db.pool,
        ledger_id,
        "changed-payload-001",
        br#"{"amount":20}"#,
        b"{}",
        &entries,
    )
    .await
    .unwrap();

    let error = post_with_raw_body(
        &db.pool,
        ledger_id,
        "changed-payload-001",
        br#"{"amount":21}"#,
        b"{}",
        &entries,
    )
    .await
    .expect_err("A changed request must not replay");

    assert_sqlstate(error, "23514");

    assert_eq!(
        wallet_totals(&db.pool, ledger_id, wallet_id).await,
        (20, 0, 1)
    );
}

#[tokio::test]
async fn invalid_raw_json_is_rejected_before_database_writes() {
    let db = common::TestDb::new().await;
    let (ledger_id, funding_id, wallet_id, _) = setup(&db.pool).await;

    let entries = [
        entry(wallet_id, Direction::Debit, 10),
        entry(funding_id, Direction::Credit, 10),
    ];

    let invalid_bodies: [&[u8]; 2] = [br#"{"amount":10,"amount":20}"#, b"not-json"];

    for raw_body in invalid_bodies {
        let error = post_with_raw_body(
            &db.pool,
            ledger_id,
            "invalid-json-request-001",
            raw_body,
            b"{}",
            &entries,
        )
        .await
        .expect_err("Invalid JSON must not reach PostgreSQL");

        assert!(
            matches!(&error, StoreError::InvalidRequestJson(_)),
            "Unexpected error: {error:?}"
        );
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
async fn route_scope_must_match_the_target_ledger() {
    let db = common::TestDb::new().await;
    let (ledger_id, funding_id, wallet_id, _) = setup(&db.pool).await;

    let entries = [
        entry(wallet_id, Direction::Debit, 10),
        entry(funding_id, Direction::Credit, 10),
    ];

    let wrong_scope = format!("POST /v1/ledgers/{}/transactions", Uuid::now_v7());

    let request = TenantPosting {
        ledger_id,
        transaction_id: Uuid::now_v7(),
        route_scope: &wrong_scope,
        idempotency_key: "wrong-tenant-route-001",
        raw_request_body: b"{}",
        response_body: b"{}",
        effective_at: Utc::now(),
        entries: &entries,
    };

    let error = post_tenant_transaction(&db.pool, &request)
        .await
        .expect_err("The route must match the ledger");

    assert!(
        matches!(&error, StoreError::InvalidRouteScope),
        "Unexpected error: {error:?}"
    );

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
