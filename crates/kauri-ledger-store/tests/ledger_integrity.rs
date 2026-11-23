mod common;

use chrono::Utc;
use kauri_ledger_core::{AccountCode, AccountType, Amount, Direction};
use kauri_ledger_store::{
    integrity::verify_ledger_integrity,
    tenant_repository::{
        TenantLedgerEntry, TenantPosting, create_ledger_account, post_tenant_transaction,
    },
};
use sqlx::PgPool;
use uuid::Uuid;

async fn create_ledger(pool: &PgPool) -> Uuid {
    let ledger_id = Uuid::now_v7();

    sqlx::query(
        "INSERT INTO ledger.ledgers (id, name) \
         VALUES ($1, 'Integrity Test Ledger')",
    )
    .bind(ledger_id)
    .execute(pool)
    .await
    .unwrap();

    ledger_id
}

async fn create_accounts(pool: &PgPool, ledger_id: Uuid) -> (Uuid, Uuid) {
    let funding_id = Uuid::now_v7();
    let wallet_id = Uuid::now_v7();

    for (id, code, allow_negative) in [
        (funding_id, "asset:funding", true),
        (wallet_id, "asset:wallet", false),
    ] {
        create_ledger_account(
            pool,
            ledger_id,
            id,
            &AccountCode::new(code).unwrap(),
            AccountType::Asset,
            "Integrity Test Account",
            allow_negative,
        )
        .await
        .unwrap();
    }

    (funding_id, wallet_id)
}

async fn post_funding(pool: &PgPool, ledger_id: Uuid, funding_id: Uuid, wallet_id: Uuid) {
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

    let route_scope = format!("POST /v1/ledgers/{ledger_id}/transactions");

    let raw_body = format!(
        r#"{{"entries":[
            {{"account_id":"{wallet_id}","side":"D","amount":100}},
            {{"account_id":"{funding_id}","side":"C","amount":100}}
        ]}}"#
    );

    let request = TenantPosting {
        ledger_id,
        transaction_id: Uuid::now_v7(),
        route_scope: &route_scope,
        idempotency_key: "integrity-funding-001",
        raw_request_body: raw_body.as_bytes(),
        response_body: br#"{"status":"posted"}"#,
        effective_at: Utc::now(),
        entries: &entries,
    };

    post_tenant_transaction(pool, &request).await.unwrap();
}

#[tokio::test]
async fn empty_ledger_has_balanced_totals() {
    let db = common::TestDb::new().await;
    let ledger_id = create_ledger(&db.pool).await;

    let report = verify_ledger_integrity(&db.pool, ledger_id)
        .await
        .unwrap()
        .unwrap();

    assert!(report.is_healthy());
    assert_eq!(report.accounts_checked, 0);
    assert_eq!(report.stored_net_total, 0);
    assert_eq!(report.journal_net_total, 0);
}

#[tokio::test]
async fn valid_posting_matches_stored_balances() {
    let db = common::TestDb::new().await;
    let ledger_id = create_ledger(&db.pool).await;
    let (funding_id, wallet_id) = create_accounts(&db.pool, ledger_id).await;

    post_funding(&db.pool, ledger_id, funding_id, wallet_id).await;

    let report = verify_ledger_integrity(&db.pool, ledger_id)
        .await
        .unwrap()
        .unwrap();

    assert!(report.is_healthy());
    assert_eq!(report.accounts_checked, 2);
    assert_eq!(report.stored_net_total, 0);
    assert_eq!(report.journal_net_total, 0);
    assert!(report.mismatches.is_empty());
}

#[tokio::test]
async fn detects_a_corrupted_account_total() {
    let db = common::TestDb::new().await;
    let ledger_id = create_ledger(&db.pool).await;
    let (funding_id, wallet_id) = create_accounts(&db.pool, ledger_id).await;

    post_funding(&db.pool, ledger_id, funding_id, wallet_id).await;

    // Deliberately simulate corruption through a privileged test
    // connection. Application roles cannot perform this update.
    sqlx::query(
        "UPDATE ledger.account_balances \
         SET debit_total = debit_total + 1 \
         WHERE ledger_id = $1 AND account_id = $2",
    )
    .bind(ledger_id)
    .bind(wallet_id)
    .execute(&db.pool)
    .await
    .unwrap();

    let report = verify_ledger_integrity(&db.pool, ledger_id)
        .await
        .unwrap()
        .unwrap();

    assert!(!report.is_healthy());
    assert_eq!(report.mismatches.len(), 1);
    assert_eq!(report.mismatches[0].account_id, wallet_id);
    assert_eq!(report.mismatches[0].journal_debit_total, 100);
    assert_eq!(report.mismatches[0].stored_debit_total, Some(101));
    assert_eq!(report.journal_net_total, 0);
    assert_eq!(report.stored_net_total, 1);
}

#[tokio::test]
async fn detects_per_account_drift_even_when_global_sum_is_zero() {
    let db = common::TestDb::new().await;
    let ledger_id = create_ledger(&db.pool).await;
    let (funding_id, wallet_id) = create_accounts(&db.pool, ledger_id).await;

    post_funding(&db.pool, ledger_id, funding_id, wallet_id).await;

    sqlx::query(
        "UPDATE ledger.account_balances \
         SET debit_total = debit_total + 1 \
         WHERE ledger_id = $1 AND account_id = $2",
    )
    .bind(ledger_id)
    .bind(wallet_id)
    .execute(&db.pool)
    .await
    .unwrap();

    sqlx::query(
        "UPDATE ledger.account_balances \
         SET credit_total = credit_total + 1 \
         WHERE ledger_id = $1 AND account_id = $2",
    )
    .bind(ledger_id)
    .bind(funding_id)
    .execute(&db.pool)
    .await
    .unwrap();

    let report = verify_ledger_integrity(&db.pool, ledger_id)
        .await
        .unwrap()
        .unwrap();

    assert_eq!(report.stored_net_total, 0);
    assert_eq!(report.journal_net_total, 0);
    assert_eq!(report.mismatches.len(), 2);
    assert!(!report.is_healthy());
}

#[tokio::test]
async fn detects_missing_account_balance_row() {
    let db = common::TestDb::new().await;
    let ledger_id = create_ledger(&db.pool).await;
    let (_, wallet_id) = create_accounts(&db.pool, ledger_id).await;

    sqlx::query(
        "DELETE FROM ledger.account_balances \
         WHERE ledger_id = $1 AND account_id = $2",
    )
    .bind(ledger_id)
    .bind(wallet_id)
    .execute(&db.pool)
    .await
    .unwrap();

    let report = verify_ledger_integrity(&db.pool, ledger_id)
        .await
        .unwrap()
        .unwrap();

    assert!(!report.is_healthy());
    assert_eq!(report.mismatches.len(), 1);
    assert_eq!(report.mismatches[0].account_id, wallet_id);
    assert_eq!(report.mismatches[0].stored_debit_total, None);
    assert_eq!(report.mismatches[0].stored_credit_total, None);
}

#[tokio::test]
async fn verification_is_scoped_to_one_ledger() {
    let db = common::TestDb::new().await;

    let first_ledger = create_ledger(&db.pool).await;
    let second_ledger = create_ledger(&db.pool).await;

    let (_, first_wallet) = create_accounts(&db.pool, first_ledger).await;

    create_accounts(&db.pool, second_ledger).await;

    sqlx::query(
        "UPDATE ledger.account_balances \
         SET debit_total = debit_total + 7 \
         WHERE ledger_id = $1 AND account_id = $2",
    )
    .bind(first_ledger)
    .bind(first_wallet)
    .execute(&db.pool)
    .await
    .unwrap();

    let first_report = verify_ledger_integrity(&db.pool, first_ledger)
        .await
        .unwrap()
        .unwrap();

    let second_report = verify_ledger_integrity(&db.pool, second_ledger)
        .await
        .unwrap()
        .unwrap();

    assert!(!first_report.is_healthy());
    assert!(second_report.is_healthy());
    assert_eq!(second_report.accounts_checked, 2);
}

#[tokio::test]
async fn nonexistent_ledger_returns_none() {
    let db = common::TestDb::new().await;

    let report = verify_ledger_integrity(&db.pool, Uuid::now_v7())
        .await
        .unwrap();

    assert!(report.is_none());
}
