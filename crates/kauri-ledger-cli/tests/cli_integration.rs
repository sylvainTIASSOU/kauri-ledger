use kauri_ledger_core::{AccountCode, AccountType, Amount, BalancedTransaction, Direction, Entry};
use kauri_ledger_store::{create_account, create_pool, record_transaction, run_migrations};
use std::process::Command;
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;

fn get_cli_binary_path() -> &'static str {
    env!("CARGO_BIN_EXE_kauri-cli")
}

async fn initialize_test_database(pool: &sqlx::PgPool) {
    sqlx::raw_sql(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../ops/postgres/bootstrap_roles.sql"
    )))
    .execute(pool)
    .await
    .expect("Failed to provision test security roles");

    run_migrations(pool)
        .await
        .expect("Failed to run migrations");
}

#[tokio::test]
async fn test_cli_verify_integrity_success() {
    let container = Postgres::default().start().await.unwrap();
    let host = container.get_host().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let database_url = format!("postgres://postgres:postgres@{}:{}/postgres", host, port);

    let pool = create_pool(&database_url).await.unwrap();
    initialize_test_database(&pool).await;

    let asset_code = AccountCode::new("asset:cash").unwrap();
    let liability_code = AccountCode::new("liability:debt").unwrap();

    create_account(&pool, &asset_code, AccountType::Asset)
        .await
        .unwrap();
    create_account(&pool, &liability_code, AccountType::Liability)
        .await
        .unwrap();

    let amount = Amount::new(5000).unwrap();
    let entries = vec![
        Entry::new(asset_code.clone(), Direction::Debit, amount),
        Entry::new(liability_code.clone(), Direction::Credit, amount),
    ];
    let tx = BalancedTransaction::new(entries).unwrap();

    record_transaction(&pool, "cli-test-1", &tx).await.unwrap();

    // Run the CLI command using the compiled binary directly
    let cli_path = get_cli_binary_path();
    let output = Command::new(cli_path)
        .args([
            "verify-integrity",
            "--database-url",
            &database_url,
            "--account",
            asset_code.as_str(),
        ])
        .output()
        .expect("Failed to execute CLI binary");

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        panic!(
            "CLI command failed with status {:?}\nstdout: {}\nstderr: {}",
            output.status.code(),
            stdout,
            stderr
        );
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("consistent"),
        "Output should indicate consistency, got: {}",
        stdout
    );
}

#[tokio::test]
async fn test_cli_verify_all_accounts() {
    let container = Postgres::default().start().await.unwrap();
    let host = container.get_host().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let database_url = format!("postgres://postgres:postgres@{}:{}/postgres", host, port);

    let pool = create_pool(&database_url).await.unwrap();
    initialize_test_database(&pool).await;

    let asset_code = AccountCode::new("asset:cash").unwrap();
    let liability_code = AccountCode::new("liability:debt").unwrap();

    create_account(&pool, &asset_code, AccountType::Asset)
        .await
        .unwrap();
    create_account(&pool, &liability_code, AccountType::Liability)
        .await
        .unwrap();

    let amount = Amount::new(3000).unwrap();
    let entries = vec![
        Entry::new(asset_code.clone(), Direction::Debit, amount),
        Entry::new(liability_code.clone(), Direction::Credit, amount),
    ];
    let tx = BalancedTransaction::new(entries).unwrap();

    record_transaction(&pool, "cli-test-all", &tx)
        .await
        .unwrap();

    // Run the CLI command without specifying an account (verifies all)
    let cli_path = get_cli_binary_path();
    let output = Command::new(cli_path)
        .args(["verify-integrity", "--database-url", &database_url])
        .output()
        .expect("Failed to execute CLI binary");

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        panic!(
            "CLI command failed with status {:?}\nstdout: {}\nstderr: {}",
            output.status.code(),
            stdout,
            stderr
        );
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("All accounts are consistent"),
        "Output should indicate all accounts are consistent, got: {}",
        stdout
    );
}
