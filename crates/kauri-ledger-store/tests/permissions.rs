mod common;

use sqlx::PgPool;
use uuid::Uuid;

fn assert_permission_denied(error: sqlx::Error) {
    let code = match error {
        sqlx::Error::Database(database_error) => {
            database_error.code().map(|code| code.into_owned())
        }
        other => panic!("Expected a database error, got: {other:?}"),
    };

    assert_eq!(code.as_deref(), Some("42501"));
}

async fn execute_as_role(
    pool: &PgPool,
    role: &str,
    statement: &'static str,
) -> Result<(), sqlx::Error> {
    let mut transaction = pool.begin().await.unwrap();

    let command = match role {
        "ledger_app" => "SET LOCAL ROLE ledger_app",
        "ledger_admin" => "SET LOCAL ROLE ledger_admin",
        "ledger_readonly" => "SET LOCAL ROLE ledger_readonly",
        _ => panic!("Unexpected test role"),
    };

    sqlx::query(command)
        .execute(&mut *transaction)
        .await
        .unwrap();

    sqlx::query(statement)
        .execute(&mut *transaction)
        .await
        .map(|_| ())
}

#[tokio::test]
async fn application_role_cannot_access_accounting_tables() {
    let db = common::TestDb::new().await;

    let forbidden_statements = [
        "SELECT COUNT(*) FROM ledger.accounts",
        "SELECT COUNT(*) FROM ledger.transactions",
        "SELECT COUNT(*) FROM ledger.entries",
        "SELECT COUNT(*) FROM ledger.account_balances",
        "INSERT INTO ledger.transactions DEFAULT VALUES",
        "INSERT INTO ledger.entries DEFAULT VALUES",
        "UPDATE ledger.account_balances SET debit_total = 1",
        "DELETE FROM ledger.transactions",
        "TRUNCATE ledger.entries",
        "SELECT ledger.close_period(
            '00000000-0000-0000-0000-000000000000'::uuid,
            NOW()
        )",
    ];

    for statement in forbidden_statements {
        let error = execute_as_role(&db.pool, "ledger_app", statement)
            .await
            .expect_err("Application role must not access protected data");

        assert_permission_denied(error);
    }
}

#[tokio::test]
async fn admin_can_freeze_and_close_periods() {
    let db = common::TestDb::new().await;
    let ledger_id = Uuid::now_v7();

    sqlx::query("INSERT INTO ledger.ledgers (id, name) VALUES ($1, $2)")
        .bind(ledger_id)
        .bind("Admin Test Ledger")
        .execute(&db.pool)
        .await
        .unwrap();

    let mut transaction = db.pool.begin().await.unwrap();

    sqlx::query("SET LOCAL ROLE ledger_admin")
        .execute(&mut *transaction)
        .await
        .unwrap();

    sqlx::query(
        "UPDATE ledger.ledgers \
         SET status = 'frozen' WHERE id = $1",
    )
    .bind(ledger_id)
    .execute(&mut *transaction)
    .await
    .unwrap();

    sqlx::query("SELECT ledger.close_period($1, $2)")
        .bind(ledger_id)
        .bind(chrono::Utc::now())
        .execute(&mut *transaction)
        .await
        .unwrap();

    transaction.commit().await.unwrap();

    let error = execute_as_role(&db.pool, "ledger_admin", "DELETE FROM ledger.entries")
        .await
        .expect_err("Administrator must not delete ledger entries");

    assert_permission_denied(error);

    let error = execute_as_role(
        &db.pool,
        "ledger_admin",
        "UPDATE ledger.period_locks SET locked_until = NOW()",
    )
    .await
    .expect_err("Administrator must use the controlled function");

    assert_permission_denied(error);
}

#[tokio::test]
async fn readonly_role_cannot_modify_accounting_data() {
    let db = common::TestDb::new().await;

    let mut transaction = db.pool.begin().await.unwrap();

    sqlx::query("SET LOCAL ROLE ledger_readonly")
        .execute(&mut *transaction)
        .await
        .unwrap();

    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM ledger.accounts")
        .fetch_one(&mut *transaction)
        .await
        .unwrap();

    assert_eq!(count, 0);
    transaction.rollback().await.unwrap();

    for statement in [
        "UPDATE ledger.ledgers SET status = 'frozen'",
        "INSERT INTO ledger.accounts DEFAULT VALUES",
        "TRUNCATE ledger.entries",
    ] {
        let error = execute_as_role(&db.pool, "ledger_readonly", statement)
            .await
            .expect_err("Read-only role must not modify data");

        assert_permission_denied(error);
    }
}

#[tokio::test]
async fn security_objects_have_trusted_ownership() {
    let db = common::TestDb::new().await;

    let owners: Vec<(String, String)> = sqlx::query_as(
        "SELECT proname::text, pg_get_userbyid(proowner) \
         FROM pg_proc \
         WHERE pronamespace = 'ledger'::regnamespace \
         AND proname IN ( \
             'guard_entry_insertion', \
             'assert_transaction_balanced', \
             'assert_period_open', \
             'close_period', \
             'reject_ledger_mutation' \
         )",
    )
    .fetch_all(&db.pool)
    .await
    .unwrap();

    assert_eq!(owners.len(), 5);

    for (function, owner) in owners {
        assert_eq!(owner, "ledger_migrator", "Unexpected owner for {function}");
    }

    let roles: Vec<(String, bool, bool, bool)> = sqlx::query_as(
        "SELECT rolname::text, rolsuper, rolcanlogin, rolbypassrls \
         FROM pg_roles \
         WHERE rolname IN ( \
             'ledger_migrator', \
             'ledger_app', \
             'ledger_admin', \
             'ledger_readonly' \
         )",
    )
    .fetch_all(&db.pool)
    .await
    .unwrap();

    assert_eq!(roles.len(), 4);

    for (role, is_superuser, can_login, bypasses_rls) in roles {
        assert!(!is_superuser, "{role} must not be superuser");
        assert!(!can_login, "{role} must not be a login role");
        assert!(!bypasses_rls, "{role} must not bypass RLS");
    }
}
