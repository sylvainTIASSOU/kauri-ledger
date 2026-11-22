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

async fn create_test_ledger(pool: &PgPool) -> Uuid {
    let ledger_id = Uuid::now_v7();

    sqlx::query("INSERT INTO ledger.ledgers (id, name) VALUES ($1, $2)")
        .bind(ledger_id)
        .bind("Account Creation Test Ledger")
        .execute(pool)
        .await
        .unwrap();

    ledger_id
}

async fn create_account_as_migrator(
    pool: &PgPool,
    ledger_id: Uuid,
    account_id: Uuid,
    code: &str,
    kind: &str,
    name: &str,
    allow_negative: bool,
) -> Result<Uuid, sqlx::Error> {
    let mut transaction = pool.begin().await?;

    sqlx::query("SET LOCAL ROLE ledger_migrator")
        .execute(&mut *transaction)
        .await?;

    let created_id: Uuid =
        sqlx::query_scalar("SELECT ledger.create_account($1, $2, $3, $4, $5, $6)")
            .bind(ledger_id)
            .bind(account_id)
            .bind(code)
            .bind(kind)
            .bind(name)
            .bind(allow_negative)
            .fetch_one(&mut *transaction)
            .await?;

    transaction.commit().await?;

    Ok(created_id)
}

#[tokio::test]
async fn creates_account_and_initial_balance_atomically() {
    let db = common::TestDb::new().await;
    let ledger_id = create_test_ledger(&db.pool).await;
    let account_id = Uuid::now_v7();

    let created = create_account_as_migrator(
        &db.pool,
        ledger_id,
        account_id,
        "asset:cash",
        "asset",
        "Cash Account",
        false,
    )
    .await
    .unwrap();

    assert_eq!(created, account_id);

    let account: (String, String, String, bool) = sqlx::query_as(
        "SELECT code, kind, normal_side::text, allow_negative \
             FROM ledger.accounts \
             WHERE ledger_id = $1 AND id = $2",
    )
    .bind(ledger_id)
    .bind(account_id)
    .fetch_one(&db.pool)
    .await
    .unwrap();

    assert_eq!(account.0, "asset:cash");
    assert_eq!(account.1, "asset");
    assert_eq!(account.2, "D");
    assert!(!account.3);

    let balance: (String, bool, i64, i64, i64, i64) = sqlx::query_as(
        "SELECT normal_side::text, allow_negative, \
             debit_total, credit_total, reserved_total, version \
             FROM ledger.account_balances \
             WHERE ledger_id = $1 AND account_id = $2",
    )
    .bind(ledger_id)
    .bind(account_id)
    .fetch_one(&db.pool)
    .await
    .unwrap();

    assert_eq!(balance, ("D".to_string(), false, 0, 0, 0, 0));
}

#[tokio::test]
async fn rejects_account_creation_on_frozen_ledgers() {
    let db = common::TestDb::new().await;
    let ledger_id = create_test_ledger(&db.pool).await;
    let account_id = Uuid::now_v7();

    sqlx::query(
        "UPDATE ledger.ledgers \
         SET status = 'frozen' WHERE id = $1",
    )
    .bind(ledger_id)
    .execute(&db.pool)
    .await
    .unwrap();

    let error = create_account_as_migrator(
        &db.pool,
        ledger_id,
        account_id,
        "asset:cash",
        "asset",
        "Cash Account",
        false,
    )
    .await
    .expect_err("Frozen ledger must reject account creation");

    assert_sqlstate(error, "23514");

    let account_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM ledger.accounts \
         WHERE ledger_id = $1",
    )
    .bind(ledger_id)
    .fetch_one(&db.pool)
    .await
    .unwrap();

    assert_eq!(account_count, 0);
}

#[tokio::test]
async fn rejects_invalid_account_properties() {
    let db = common::TestDb::new().await;
    let ledger_id = create_test_ledger(&db.pool).await;

    let invalid_cases = [
        ("Asset:cash", "asset", "Cash Account"),
        ("asset:cash", "unknown", "Cash Account"),
        ("asset:cash", "asset", "   "),
    ];

    for (code, kind, name) in invalid_cases {
        let error = create_account_as_migrator(
            &db.pool,
            ledger_id,
            Uuid::now_v7(),
            code,
            kind,
            name,
            false,
        )
        .await
        .expect_err("Invalid account data must be rejected");

        assert_sqlstate(error, "23514");
    }

    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM ledger.accounts \
         WHERE ledger_id = $1",
    )
    .bind(ledger_id)
    .fetch_one(&db.pool)
    .await
    .unwrap();

    assert_eq!(count, 0);
}

#[tokio::test]
async fn scopes_account_codes_to_their_ledgers() {
    let db = common::TestDb::new().await;
    let first_ledger = create_test_ledger(&db.pool).await;
    let second_ledger = create_test_ledger(&db.pool).await;

    let shared_account_id = Uuid::now_v7();

    for ledger_id in [first_ledger, second_ledger] {
        create_account_as_migrator(
            &db.pool,
            ledger_id,
            shared_account_id,
            "asset:cash",
            "asset",
            "Cash Account",
            false,
        )
        .await
        .unwrap();
    }

    let error = create_account_as_migrator(
        &db.pool,
        first_ledger,
        Uuid::now_v7(),
        "asset:cash",
        "asset",
        "Duplicate Cash Account",
        false,
    )
    .await
    .expect_err("Duplicate codes within the same ledger must fail");

    assert_sqlstate(error, "23505");

    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM ledger.accounts \
         WHERE id = $1",
    )
    .bind(shared_account_id)
    .fetch_one(&db.pool)
    .await
    .unwrap();

    assert_eq!(count, 2);
}

#[tokio::test]
async fn application_role_cannot_execute_privileged_account_creation() {
    let db = common::TestDb::new().await;
    let ledger_id = create_test_ledger(&db.pool).await;

    let mut transaction = db.pool.begin().await.unwrap();

    sqlx::query("SET LOCAL ROLE ledger_app")
        .execute(&mut *transaction)
        .await
        .unwrap();

    let error =
        sqlx::query_scalar::<_, Uuid>("SELECT ledger.create_account($1, $2, $3, $4, $5, $6)")
            .bind(ledger_id)
            .bind(Uuid::now_v7())
            .bind("asset:cash")
            .bind("asset")
            .bind("Cash Account")
            .bind(false)
            .fetch_one(&mut *transaction)
            .await
            .expect_err("Application role must not execute this function yet");

    assert_sqlstate(error, "42501");

    transaction.rollback().await.unwrap();
}

#[tokio::test]
async fn explicit_rollback_removes_account_and_balance() {
    let db = common::TestDb::new().await;
    let ledger_id = create_test_ledger(&db.pool).await;
    let account_id = Uuid::now_v7();

    let mut transaction = db.pool.begin().await.unwrap();

    sqlx::query("SET LOCAL ROLE ledger_migrator")
        .execute(&mut *transaction)
        .await
        .unwrap();

    let created_id: Uuid =
        sqlx::query_scalar("SELECT ledger.create_account($1, $2, $3, $4, $5, $6)")
            .bind(ledger_id)
            .bind(account_id)
            .bind("liability:debt")
            .bind("liability")
            .bind("Debt Account")
            .bind(false)
            .fetch_one(&mut *transaction)
            .await
            .unwrap();

    assert_eq!(created_id, account_id);

    transaction.rollback().await.unwrap();

    let account_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM ledger.accounts \
         WHERE ledger_id = $1 AND id = $2",
    )
    .bind(ledger_id)
    .bind(account_id)
    .fetch_one(&db.pool)
    .await
    .unwrap();

    let balance_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM ledger.account_balances \
         WHERE ledger_id = $1 AND account_id = $2",
    )
    .bind(ledger_id)
    .bind(account_id)
    .fetch_one(&db.pool)
    .await
    .unwrap();

    assert_eq!(account_count, 0);
    assert_eq!(balance_count, 0);
}
