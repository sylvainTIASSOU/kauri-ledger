mod common;

use kauri_ledger_core::{AccountCode, AccountType};
use kauri_ledger_store::{
    api_auth::{ApiAccess, ApiKeyProof, ApiScope, check_api_access},
    tenant_repository::create_ledger_account,
};
use sqlx::PgPool;
use uuid::Uuid;

type TestBalanceRow = (Uuid, String, String, i64, i64, i64, i64);

async fn create_test_ledger(pool: &PgPool, name: &str) -> Uuid {
    let ledger_id = Uuid::now_v7();

    sqlx::query("INSERT INTO ledger.ledgers (id, name) VALUES ($1, $2)")
        .bind(ledger_id)
        .bind(name)
        .execute(pool)
        .await
        .unwrap();

    ledger_id
}

#[tokio::test]
async fn api_keys_enforce_scope_tenant_and_revocation() {
    let db = common::TestDb::new().await;

    let first_ledger = create_test_ledger(&db.pool, "Authorized Ledger").await;

    let other_ledger = create_test_ledger(&db.pool, "Foreign Ledger").await;

    let account_id = Uuid::now_v7();

    create_ledger_account(
        &db.pool,
        first_ledger,
        account_id,
        &AccountCode::new("asset:wallet").unwrap(),
        AccountType::Asset,
        "Test Wallet",
        false,
    )
    .await
    .unwrap();

    let token = format!("lk_test_{}", "A".repeat(43));
    let authorization = format!("Bearer {token}");

    let proof = ApiKeyProof::from_authorization(&authorization).unwrap();

    let key_id = Uuid::now_v7();

    // Test fixtures provision keys through a privileged connection.
    // Production key provisioning must use the private CLI.
    sqlx::query(
        r#"
        INSERT INTO ledger.api_keys (
            id, key_hash, prefix, last4, scopes, label
        )
        VALUES (
            $1, $2, $3, $4, $5, $6
        )
        "#,
    )
    .bind(key_id)
    .bind(proof.hash_bytes())
    .bind(proof.prefix())
    .bind(&token[token.len() - 4..])
    .bind(vec!["read"])
    .bind("Read-only test key")
    .execute(&db.pool)
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO ledger.api_key_ledgers \
         (api_key_id, ledger_id) VALUES ($1, $2)",
    )
    .bind(key_id)
    .bind(first_ledger)
    .execute(&db.pool)
    .await
    .unwrap();

    // A valid key can access its own ledger with its own scope.
    assert_eq!(
        check_api_access(&db.pool, &proof, first_ledger, ApiScope::Read,)
            .await
            .unwrap(),
        ApiAccess::Allowed
    );

    // The same key cannot perform write operations.
    assert_eq!(
        check_api_access(&db.pool, &proof, first_ledger, ApiScope::Write,)
            .await
            .unwrap(),
        ApiAccess::Forbidden
    );

    // A foreign ledger must not be disclosed.
    assert_eq!(
        check_api_access(&db.pool, &proof, other_ledger, ApiScope::Read,)
            .await
            .unwrap(),
        ApiAccess::NotFound
    );

    // One key cannot be associated with two ledgers in v1.
    let second_mapping = sqlx::query(
        "INSERT INTO ledger.api_key_ledgers \
         (api_key_id, ledger_id) VALUES ($1, $2)",
    )
    .bind(key_id)
    .bind(other_ledger)
    .execute(&db.pool)
    .await;

    assert!(
        second_mapping.is_err(),
        "A v1 API key must have only one ledger"
    );

    // Verify the authorized read wrapper as ledger_app.
    let mut transaction = db.pool.begin().await.unwrap();

    sqlx::query("SET LOCAL ROLE ledger_app")
        .execute(&mut *transaction)
        .await
        .unwrap();

    let balance: TestBalanceRow = sqlx::query_as(
        r#"
        SELECT
            account_id,
            account_code,
            account_status,
            posted_total,
            reserved_total,
            available_total,
            balance_version
        FROM ledger.authorized_get_balance(
            $1, $2, $3, $4
        )
        "#,
    )
    .bind(proof.prefix())
    .bind(proof.hash_bytes())
    .bind(first_ledger)
    .bind("asset:wallet")
    .fetch_one(&mut *transaction)
    .await
    .unwrap();

    assert_eq!(balance.0, account_id);
    assert_eq!(balance.1, "asset:wallet");
    assert_eq!(balance.2, "active");
    assert_eq!(balance.3, 0);
    assert_eq!(balance.4, 0);
    assert_eq!(balance.5, 0);
    assert_eq!(balance.6, 0);

    transaction.commit().await.unwrap();

    // Runtime role must never read the raw key records.
    let mut restricted = db.pool.begin().await.unwrap();

    sqlx::query("SET LOCAL ROLE ledger_app")
        .execute(&mut *restricted)
        .await
        .unwrap();

    let forbidden_select = sqlx::query("SELECT key_hash FROM ledger.api_keys")
        .fetch_all(&mut *restricted)
        .await;

    assert!(
        forbidden_select.is_err(),
        "ledger_app must not read API key hashes"
    );

    restricted.rollback().await.unwrap();

    // The new authorized wrapper is executable by ledger_app.
    // The original privileged posting function is not.
    let direct_post_allowed: bool = sqlx::query_scalar(
        r#"
        SELECT has_function_privilege(
            'ledger_app',
            'ledger.post_transaction(
                uuid, uuid, text, text, bytea, bytea,
                timestamptz, jsonb
            )',
            'EXECUTE'
        )
        "#,
    )
    .fetch_one(&db.pool)
    .await
    .unwrap();

    let authorized_post_allowed: bool = sqlx::query_scalar(
        r#"
        SELECT has_function_privilege(
            'ledger_app',
            'ledger.authorized_post_transaction(
                text, bytea, uuid, uuid, text, text,
                bytea, bytea, timestamptz, jsonb
            )',
            'EXECUTE'
        )
        "#,
    )
    .fetch_one(&db.pool)
    .await
    .unwrap();

    assert!(!direct_post_allowed);
    assert!(authorized_post_allowed);

    // Revocation must take effect without a cache delay.
    sqlx::query(
        "UPDATE ledger.api_keys \
         SET revoked_at = NOW() WHERE id = $1",
    )
    .bind(key_id)
    .execute(&db.pool)
    .await
    .unwrap();

    assert_eq!(
        check_api_access(&db.pool, &proof, first_ledger, ApiScope::Read,)
            .await
            .unwrap(),
        ApiAccess::Unauthenticated
    );

    // Expiration is also an authentication failure.
    sqlx::query(
        "UPDATE ledger.api_keys \
         SET revoked_at = NULL, \
             expires_at = NOW() - INTERVAL '1 hour' \
         WHERE id = $1",
    )
    .bind(key_id)
    .execute(&db.pool)
    .await
    .unwrap();

    assert_eq!(
        check_api_access(&db.pool, &proof, first_ledger, ApiScope::Read,)
            .await
            .unwrap(),
        ApiAccess::Unauthenticated
    );
}
