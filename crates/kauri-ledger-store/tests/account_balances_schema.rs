mod common;

use sqlx::PgPool;
use uuid::Uuid;

fn assert_sqlstate(error: sqlx::Error, expected: &str) {
    let code = match error {
        sqlx::Error::Database(database_error) => {
            database_error.code().map(|code| code.into_owned())
        }
        other => panic!("Expected a database error, got: {other:?}"),
    };

    assert_eq!(code.as_deref(), Some(expected));
}

async fn create_ledger(pool: &PgPool) -> Uuid {
    let id = Uuid::now_v7();

    sqlx::query("INSERT INTO ledger.ledgers (id, name) VALUES ($1, $2)")
        .bind(id)
        .bind("Test Ledger")
        .execute(pool)
        .await
        .unwrap();

    id
}

async fn create_account(
    pool: &PgPool,
    ledger_id: Uuid,
    account_id: Uuid,
    code: &str,
    kind: &str,
    normal_side: &str,
    allow_negative: bool,
) {
    sqlx::query(
        "INSERT INTO ledger.accounts \
         (ledger_id, id, code, kind, normal_side, \
          allow_negative, name) \
         VALUES ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(ledger_id)
    .bind(account_id)
    .bind(code)
    .bind(kind)
    .bind(normal_side)
    .bind(allow_negative)
    .bind("Test Account")
    .execute(pool)
    .await
    .unwrap();
}

async fn create_balance(
    pool: &PgPool,
    ledger_id: Uuid,
    account_id: Uuid,
    normal_side: &str,
    allow_negative: bool,
) {
    sqlx::query(
        "INSERT INTO ledger.account_balances \
         (ledger_id, account_id, normal_side, allow_negative) \
         VALUES ($1, $2, $3, $4)",
    )
    .bind(ledger_id)
    .bind(account_id)
    .bind(normal_side)
    .bind(allow_negative)
    .execute(pool)
    .await
    .unwrap();
}

#[tokio::test]
async fn isolates_accounts_between_ledgers() {
    let db = common::TestDb::new().await;

    let first_ledger = create_ledger(&db.pool).await;
    let second_ledger = create_ledger(&db.pool).await;
    let account_id = Uuid::now_v7();

    create_account(
        &db.pool,
        first_ledger,
        account_id,
        "asset:cash",
        "asset",
        "D",
        false,
    )
    .await;

    create_account(
        &db.pool,
        second_ledger,
        account_id,
        "asset:cash",
        "asset",
        "D",
        false,
    )
    .await;

    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM ledger.accounts WHERE id = $1")
        .bind(account_id)
        .fetch_one(&db.pool)
        .await
        .unwrap();

    assert_eq!(count, 2);

    let error = sqlx::query(
        "INSERT INTO ledger.accounts \
         (ledger_id, id, code, kind, normal_side, name) \
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(first_ledger)
    .bind(Uuid::now_v7())
    .bind("asset:cash")
    .bind("asset")
    .bind("D")
    .bind("Duplicate Account")
    .execute(&db.pool)
    .await
    .expect_err("Account codes must be unique within a ledger");

    assert_sqlstate(error, "23505");
}

#[tokio::test]
async fn rejects_invalid_account_properties() {
    let db = common::TestDb::new().await;
    let ledger_id = create_ledger(&db.pool).await;

    let invalid_cases = [
        ("Asset:cash", "asset", "D", "active"),
        ("asset:cash", "unknown", "D", "active"),
        ("asset:cash", "asset", "C", "active"),
        ("liability:debt", "liability", "D", "active"),
        ("asset:cash", "asset", "D", "suspended"),
        ("", "asset", "D", "active"),
    ];

    for (code, kind, normal_side, status) in invalid_cases {
        let error = sqlx::query(
            "INSERT INTO ledger.accounts \
             (ledger_id, id, code, kind, normal_side, name, status) \
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(ledger_id)
        .bind(Uuid::now_v7())
        .bind(code)
        .bind(kind)
        .bind(normal_side)
        .bind("Test Account")
        .bind(status)
        .execute(&db.pool)
        .await
        .expect_err("Invalid account properties must be rejected");

        assert_sqlstate(error, "23514");
    }

    let error = sqlx::query(
        "INSERT INTO ledger.accounts \
         (ledger_id, id, code, kind, normal_side, name) \
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(ledger_id)
    .bind(Uuid::now_v7())
    .bind("asset:valid")
    .bind("asset")
    .bind("D")
    .bind("   ")
    .execute(&db.pool)
    .await
    .expect_err("Blank account names must be rejected");

    assert_sqlstate(error, "23514");
}

#[tokio::test]
async fn rejects_cross_ledger_balances_and_policy_mismatches() {
    let db = common::TestDb::new().await;

    let first_ledger = create_ledger(&db.pool).await;
    let second_ledger = create_ledger(&db.pool).await;
    let account_id = Uuid::now_v7();

    create_account(
        &db.pool,
        first_ledger,
        account_id,
        "asset:cash",
        "asset",
        "D",
        false,
    )
    .await;

    let error = sqlx::query(
        "INSERT INTO ledger.account_balances \
         (ledger_id, account_id, normal_side, allow_negative) \
         VALUES ($1, $2, $3, $4)",
    )
    .bind(second_ledger)
    .bind(account_id)
    .bind("D")
    .bind(false)
    .execute(&db.pool)
    .await
    .expect_err("Cross-ledger balance references must be rejected");

    assert_sqlstate(error, "23503");

    create_balance(&db.pool, first_ledger, account_id, "D", false).await;

    let error = sqlx::query(
        "UPDATE ledger.account_balances \
         SET normal_side = 'C' \
         WHERE ledger_id = $1 AND account_id = $2",
    )
    .bind(first_ledger)
    .bind(account_id)
    .execute(&db.pool)
    .await
    .expect_err("Account balance policy must match its account");

    assert_sqlstate(error, "23503");

    let error = sqlx::query(
        "DELETE FROM ledger.accounts \
         WHERE ledger_id = $1 AND id = $2",
    )
    .bind(first_ledger)
    .bind(account_id)
    .execute(&db.pool)
    .await
    .expect_err("Accounts with balances must not be deleted");

    assert_sqlstate(error, "23503");

    sqlx::query(
        "UPDATE ledger.accounts \
         SET allow_negative = TRUE \
         WHERE ledger_id = $1 AND id = $2",
    )
    .bind(first_ledger)
    .bind(account_id)
    .execute(&db.pool)
    .await
    .unwrap();

    let allow_negative: bool = sqlx::query_scalar(
        "SELECT allow_negative \
         FROM ledger.account_balances \
         WHERE ledger_id = $1 AND account_id = $2",
    )
    .bind(first_ledger)
    .bind(account_id)
    .fetch_one(&db.pool)
    .await
    .unwrap();

    assert!(allow_negative);
}

#[tokio::test]
async fn enforces_natural_balance_and_reservation_limits() {
    let db = common::TestDb::new().await;
    let ledger_id = create_ledger(&db.pool).await;
    let account_id = Uuid::now_v7();

    create_account(
        &db.pool,
        ledger_id,
        account_id,
        "asset:cash",
        "asset",
        "D",
        false,
    )
    .await;

    create_balance(&db.pool, ledger_id, account_id, "D", false).await;

    sqlx::query(
        "UPDATE ledger.account_balances \
         SET debit_total = 100, \
             credit_total = 30, \
             reserved_total = 60 \
         WHERE ledger_id = $1 AND account_id = $2",
    )
    .bind(ledger_id)
    .bind(account_id)
    .execute(&db.pool)
    .await
    .unwrap();

    let invalid_updates = [
        "UPDATE ledger.account_balances \
         SET reserved_total = 71 \
         WHERE ledger_id = $1 AND account_id = $2",
        "UPDATE ledger.account_balances \
         SET credit_total = 101 \
         WHERE ledger_id = $1 AND account_id = $2",
        "UPDATE ledger.account_balances \
         SET debit_total = 4000000000000000001 \
         WHERE ledger_id = $1 AND account_id = $2",
        "UPDATE ledger.account_balances \
         SET reserved_total = 4000000000000000001 \
         WHERE ledger_id = $1 AND account_id = $2",
        "UPDATE ledger.account_balances \
         SET version = -1 \
         WHERE ledger_id = $1 AND account_id = $2",
    ];

    for statement in invalid_updates {
        let error = sqlx::query(statement)
            .bind(ledger_id)
            .bind(account_id)
            .execute(&db.pool)
            .await
            .expect_err("Invalid balance values must be rejected");

        assert_sqlstate(error, "23514");
    }

    let totals: (i64, i64, i64) = sqlx::query_as(
        "SELECT debit_total, credit_total, reserved_total \
         FROM ledger.account_balances \
         WHERE ledger_id = $1 AND account_id = $2",
    )
    .bind(ledger_id)
    .bind(account_id)
    .fetch_one(&db.pool)
    .await
    .unwrap();

    assert_eq!(totals, (100, 30, 60));
}

#[tokio::test]
async fn allows_negative_balances_only_when_configured() {
    let db = common::TestDb::new().await;
    let ledger_id = create_ledger(&db.pool).await;
    let account_id = Uuid::now_v7();

    create_account(
        &db.pool,
        ledger_id,
        account_id,
        "asset:overdraft",
        "asset",
        "D",
        true,
    )
    .await;

    create_balance(&db.pool, ledger_id, account_id, "D", true).await;

    sqlx::query(
        "UPDATE ledger.account_balances \
         SET credit_total = 100, reserved_total = 50 \
         WHERE ledger_id = $1 AND account_id = $2",
    )
    .bind(ledger_id)
    .bind(account_id)
    .execute(&db.pool)
    .await
    .unwrap();

    let error = sqlx::query(
        "UPDATE ledger.accounts \
         SET allow_negative = FALSE \
         WHERE ledger_id = $1 AND id = $2",
    )
    .bind(ledger_id)
    .bind(account_id)
    .execute(&db.pool)
    .await
    .expect_err("Disabling overdrafts must not invalidate a balance");

    assert_sqlstate(error, "23514");

    let account_policy: bool = sqlx::query_scalar(
        "SELECT allow_negative FROM ledger.accounts \
         WHERE ledger_id = $1 AND id = $2",
    )
    .bind(ledger_id)
    .bind(account_id)
    .fetch_one(&db.pool)
    .await
    .unwrap();

    let balance_policy: bool = sqlx::query_scalar(
        "SELECT allow_negative FROM ledger.account_balances \
         WHERE ledger_id = $1 AND account_id = $2",
    )
    .bind(ledger_id)
    .bind(account_id)
    .fetch_one(&db.pool)
    .await
    .unwrap();

    assert!(account_policy);
    assert!(balance_policy);
}
