use kauri_ledger_store::{create_pool, run_migrations};
use serde_json::Value;
use sqlx::PgPool;
use std::process::Output;
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;
use tokio::process::Command;
use uuid::Uuid;

async fn run_audit(database_url: &str, arguments: &[&str]) -> Output {
    let mut cli_path = std::env::current_exe().unwrap();
    cli_path.pop();
    cli_path.pop();
    cli_path.push("kauri-cli");

    Command::new(cli_path)
        .args(["audit-ledger", "--database-url", database_url])
        .args(arguments)
        .output()
        .await
        .expect("Failed to run kauri-cli")
}

async fn create_test_ledger(pool: &PgPool, ledger_id: Uuid, account_id: Uuid, label: &str) {
    sqlx::query(
        "INSERT INTO ledger.ledgers (id, name) \
         VALUES ($1, $2)",
    )
    .bind(ledger_id)
    .bind(format!("CLI Audit Ledger {label}"))
    .execute(pool)
    .await
    .unwrap();

    let account_code = format!("asset:wallet:{label}");

    let created_id: Uuid = sqlx::query_scalar(
        "SELECT ledger.create_account(\
            $1, $2, $3, $4, $5, $6\
        )",
    )
    .bind(ledger_id)
    .bind(account_id)
    .bind(account_code)
    .bind("asset")
    .bind("CLI Audit Account")
    .bind(false)
    .fetch_one(pool)
    .await
    .unwrap();

    assert_eq!(created_id, account_id);
}

#[tokio::test]
async fn cli_audits_selected_and_all_ledgers() {
    let container = Postgres::default().start().await.unwrap();

    let host = container.get_host().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();

    let database_url = format!("postgres://postgres:postgres@{host}:{port}/postgres");

    let pool = create_pool(&database_url).await.unwrap();

    // Bootstrap the required PostgreSQL roles before migrations.
    sqlx::raw_sql(include_str!("../../../ops/postgres/bootstrap_roles.sql"))
        .execute(&pool)
        .await
        .expect("Failed to bootstrap PostgreSQL roles");

    run_migrations(&pool)
        .await
        .expect("Failed to run migrations");

    let first_ledger = Uuid::now_v7();
    let second_ledger = Uuid::now_v7();
    let first_account = Uuid::now_v7();
    let second_account = Uuid::now_v7();

    create_test_ledger(&pool, first_ledger, first_account, "first").await;

    create_test_ledger(&pool, second_ledger, second_account, "second").await;

    let first_id = first_ledger.to_string();

    // A healthy single-ledger audit must exit successfully.
    let healthy_output =
        run_audit(&database_url, &["--ledger", &first_id, "--format", "json"]).await;

    assert_eq!(healthy_output.status.code(), Some(0));

    let healthy_json: Value = serde_json::from_slice(&healthy_output.stdout).unwrap();

    assert_eq!(healthy_json["healthy"], true);
    assert_eq!(healthy_json["ledgers_checked"], 1);
    assert_eq!(healthy_json["ledgers"][0]["healthy"], true);

    // Simulate an incorrect stored balance.
    sqlx::query(
        "UPDATE ledger.account_balances \
         SET debit_total = debit_total + 1 \
         WHERE ledger_id = $1 AND account_id = $2",
    )
    .bind(first_ledger)
    .bind(first_account)
    .execute(&pool)
    .await
    .unwrap();

    // An unhealthy ledger must return exit code 2.
    let unhealthy_output =
        run_audit(&database_url, &["--ledger", &first_id, "--format", "text"]).await;

    assert_eq!(unhealthy_output.status.code(), Some(2));

    let text_output = String::from_utf8(unhealthy_output.stdout).unwrap();

    assert!(text_output.contains("UNHEALTHY"));
    assert!(text_output.contains("ACCOUNT_MISMATCH"));

    // The all-ledger audit must report both healthy and unhealthy
    // ledgers without mixing their accounting data.
    let all_output = run_audit(&database_url, &["--format", "json"]).await;

    assert_eq!(all_output.status.code(), Some(2));

    let all_json: Value = serde_json::from_slice(&all_output.stdout).unwrap();

    assert_eq!(all_json["healthy"], false);
    assert_eq!(all_json["ledgers_checked"], 2);

    let ledgers = all_json["ledgers"].as_array().unwrap();

    let first_report = ledgers
        .iter()
        .find(|item| item["ledger_id"].as_str() == Some(first_id.as_str()))
        .unwrap();

    let second_id = second_ledger.to_string();

    let second_report = ledgers
        .iter()
        .find(|item| item["ledger_id"].as_str() == Some(second_id.as_str()))
        .unwrap();

    assert_eq!(first_report["healthy"], false);
    assert_eq!(second_report["healthy"], true);

    // An unknown ledger must produce a machine-readable error.
    let unknown_id = Uuid::now_v7().to_string();

    let missing_output = run_audit(
        &database_url,
        &["--ledger", &unknown_id, "--format", "json"],
    )
    .await;

    assert_eq!(missing_output.status.code(), Some(3));

    let missing_json: Value = serde_json::from_slice(&missing_output.stdout).unwrap();

    assert_eq!(missing_json["error"], "LEDGER_NOT_FOUND");
}
