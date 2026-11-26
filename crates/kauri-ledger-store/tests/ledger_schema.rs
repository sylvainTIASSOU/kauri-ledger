mod common;

use uuid::Uuid;

fn assert_sqlstate(error: sqlx::Error, expected: &str) {
    let actual = match error {
        sqlx::Error::Database(database_error) => {
            database_error.code().map(|code| code.into_owned())
        }
        other => panic!("Expected a PostgreSQL error, got: {other:?}"),
    };

    assert_eq!(actual.as_deref(), Some(expected));
}

#[tokio::test]
async fn creates_ledger_with_valid_defaults() {
    let db = common::TestDb::new().await;
    let ledger_id = Uuid::now_v7();

    sqlx::query("INSERT INTO ledger.ledgers (id, name) VALUES ($1, $2)")
        .bind(ledger_id)
        .bind("Test Ledger")
        .execute(&db.pool)
        .await
        .unwrap();

    let (name, currency, status): (String, String, String) = sqlx::query_as(
        "SELECT name, currency::text, status \
             FROM ledger.ledgers WHERE id = $1",
    )
    .bind(ledger_id)
    .fetch_one(&db.pool)
    .await
    .unwrap();

    assert_eq!(name, "Test Ledger");
    assert_eq!(currency, "XOF");
    assert_eq!(status, "active");

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
async fn rejects_invalid_ledger_data() {
    let db = common::TestDb::new().await;

    let invalid_cases = [
        ("", "XOF", "active"),
        ("   ", "XOF", "active"),
        ("Valid Ledger", "EUR", "active"),
        ("Valid Ledger", "XOF", "paused"),
    ];

    for (name, currency, status) in invalid_cases {
        let error = sqlx::query(
            "INSERT INTO ledger.ledgers \
             (id, name, currency, status) \
             VALUES ($1, $2, $3, $4)",
        )
        .bind(Uuid::now_v7())
        .bind(name)
        .bind(currency)
        .bind(status)
        .execute(&db.pool)
        .await
        .expect_err("Invalid ledger data must be rejected");

        assert_sqlstate(error, "23514");
    }

    let error = sqlx::query(
        "INSERT INTO ledger.ledgers (id, name) \
         VALUES ($1, $2)",
    )
    .bind(Uuid::now_v7())
    .bind("A".repeat(201))
    .execute(&db.pool)
    .await
    .expect_err("Ledger names longer than 200 characters must fail");

    assert_sqlstate(error, "23514");

    let ledger_id = Uuid::now_v7();

    sqlx::query(
        "INSERT INTO ledger.ledgers (id, name) \
         VALUES ($1, $2)",
    )
    .bind(ledger_id)
    .bind("Original Ledger")
    .execute(&db.pool)
    .await
    .unwrap();

    let error = sqlx::query(
        "INSERT INTO ledger.ledgers (id, name) \
         VALUES ($1, $2)",
    )
    .bind(ledger_id)
    .bind("Duplicate Ledger")
    .execute(&db.pool)
    .await
    .expect_err("Duplicate ledger IDs must be rejected");

    assert_sqlstate(error, "23505");
}
