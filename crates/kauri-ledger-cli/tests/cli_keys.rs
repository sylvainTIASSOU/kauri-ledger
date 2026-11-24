use kauri_ledger_store::{
    api_auth::{ApiAccess, ApiKeyProof, ApiScope, check_api_access},
    create_pool, run_migrations,
};
use serde_json::Value;
use sqlx::PgPool;
use std::{path::PathBuf, process::Output};
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;
use tokio::process::Command;
use uuid::Uuid;

fn cli_binary_path() -> PathBuf {
    let mut path = std::env::current_exe().unwrap();

    path.pop();
    path.pop();
    path.push("kauri-cli");

    path
}

async fn run_cli(database_url: &str, args: &[&str]) -> Output {
    Command::new(cli_binary_path())
        .args(args)
        .env("LEDGER_ADMIN_DATABASE_URL", database_url)
        .env("LEDGER_ACTOR", "integration-test-operator")
        .output()
        .await
        .expect("Failed to execute kauri-cli")
}

async fn create_ledger(pool: &PgPool, name: &str) -> Uuid {
    let ledger_id = Uuid::now_v7();

    sqlx::query(
        "INSERT INTO ledger.ledgers (id, name) \
         VALUES ($1, $2)",
    )
    .bind(ledger_id)
    .bind(name)
    .execute(pool)
    .await
    .unwrap();

    ledger_id
}

#[tokio::test]
async fn cli_creates_lists_and_revokes_api_keys() {
    let container = Postgres::default().start().await.unwrap();

    let host = container.get_host().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();

    let database_url = format!("postgres://postgres:postgres@{host}:{port}/postgres");

    let pool = create_pool(&database_url).await.unwrap();

    sqlx::raw_sql(include_str!("../../../ops/postgres/bootstrap_roles.sql"))
        .execute(&pool)
        .await
        .unwrap();

    run_migrations(&pool).await.unwrap();

    let own_ledger = create_ledger(&pool, "CLI Key Lifecycle A").await;

    let other_ledger = create_ledger(&pool, "CLI Key Lifecycle B").await;

    let own_id = own_ledger.to_string();

    // 1. Create a read-only API key using the real CLI.
    let created = run_cli(
        &database_url,
        &[
            "keys",
            "create",
            "--ledger",
            &own_id,
            "--role",
            "read",
            "--label",
            "reporting-service",
            "--environment",
            "test",
            "--expires",
            "never",
        ],
    )
    .await;

    assert!(
        created.status.success(),
        "Create failed: {}",
        String::from_utf8_lossy(&created.stderr)
    );

    let token = String::from_utf8(created.stdout).unwrap().trim().to_owned();

    assert_eq!(token.len(), 51);
    assert!(token.starts_with("lk_test_"));

    let proof = ApiKeyProof::from_authorization(&format!("Bearer {token}")).unwrap();

    // 2. The key works only with the correct ledger and scope.
    assert_eq!(
        check_api_access(&pool, &proof, own_ledger, ApiScope::Read,)
            .await
            .unwrap(),
        ApiAccess::Allowed
    );

    assert_eq!(
        check_api_access(&pool, &proof, own_ledger, ApiScope::Write,)
            .await
            .unwrap(),
        ApiAccess::Forbidden
    );

    assert_eq!(
        check_api_access(&pool, &proof, other_ledger, ApiScope::Read,)
            .await
            .unwrap(),
        ApiAccess::NotFound
    );

    // 3. Listing must never reveal the raw secret.
    let listed = run_cli(&database_url, &["keys", "list", "--ledger", &own_id]).await;

    assert!(listed.status.success());

    let listing_text = String::from_utf8(listed.stdout).unwrap();

    assert!(!listing_text.contains(&token));

    let listing: Value = serde_json::from_str(&listing_text).unwrap();

    let keys = listing.as_array().unwrap();

    assert_eq!(keys.len(), 1);
    assert_eq!(keys[0]["ledger_id"], own_id);
    assert_eq!(keys[0]["label"], "reporting-service");
    assert_eq!(keys[0]["scopes"][0], "read");
    assert!(keys[0]["revoked_at"].is_null());

    let key_id = Uuid::parse_str(keys[0]["id"].as_str().unwrap()).unwrap();

    // 4. Verify that only a SHA-256 digest is persisted.
    let stored: (Vec<u8>, String, String) = sqlx::query_as(
        r#"
            SELECT key_hash, prefix, last4
            FROM ledger.api_keys
            WHERE id = $1
            "#,
    )
    .bind(key_id)
    .fetch_one(&pool)
    .await
    .unwrap();

    assert_eq!(stored.0.len(), 32);
    assert_eq!(stored.0.as_slice(), proof.hash_bytes(),);
    assert_eq!(stored.1, &token[..16]);
    assert_eq!(stored.2, &token[token.len() - 4..],);

    // 5. Revoke the key through the real CLI.
    let key_id_string = key_id.to_string();

    let revoked = run_cli(&database_url, &["keys", "revoke", "--id", &key_id_string]).await;

    assert!(revoked.status.success());

    // The authorization database sees revocation immediately.
    assert_eq!(
        check_api_access(&pool, &proof, own_ledger, ApiScope::Read,)
            .await
            .unwrap(),
        ApiAccess::Unauthenticated
    );

    // 6. Retrying revocation must be idempotent.
    let repeated = run_cli(&database_url, &["keys", "revoke", "--id", &key_id_string]).await;

    assert!(repeated.status.success());

    assert!(String::from_utf8_lossy(&repeated.stdout).contains("ALREADY_REVOKED"));

    // 7. Creation and revocation are both auditable.
    let events: Vec<(String, String)> = sqlx::query_as(
        r#"
            SELECT action, actor
            FROM ledger.audit_log
            WHERE target_id = $1
            ORDER BY id
            "#,
    )
    .bind(key_id)
    .fetch_all(&pool)
    .await
    .unwrap();

    assert_eq!(
        events,
        vec![
            (
                "api_key_created".to_owned(),
                "integration-test-operator".to_owned(),
            ),
            (
                "api_key_revoked".to_owned(),
                "integration-test-operator".to_owned(),
            ),
        ]
    );

    // An idempotent revocation creates no duplicate audit event.
    assert_eq!(events.len(), 2);
}
