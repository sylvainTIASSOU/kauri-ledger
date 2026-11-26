use axum::{
    Router,
    body::Body,
    http::{Method, Request, StatusCode},
};
use http_body_util::BodyExt;
use kauri_ledger_api::{app::build_router_with_ledger, controllers::ledger::LedgerApiState};
use kauri_ledger_core::{AccountCode, AccountType};
use kauri_ledger_store::{
    authorized_repository::assert_restricted_runtime_pool, create_pool, run_migrations,
    tenant_repository::create_ledger_account,
};
use serde_json::Value;
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;
use tower::ServiceExt;
use uuid::Uuid;

async fn send(
    app: &Router,
    method: Method,
    uri: &str,
    token: Option<&str>,
    idempotency_key: Option<&str>,
    body: &str,
) -> (StatusCode, Vec<u8>) {
    let mut request = Request::builder().method(method).uri(uri);
    if let Some(token) = token {
        request = request.header("authorization", format!("Bearer {token}"));
    }
    if let Some(key) = idempotency_key {
        request = request.header("idempotency-key", key);
    }
    if !body.is_empty() {
        request = request.header("content-type", "application/json");
    }
    let request = request.body(Body::from(body.to_owned())).unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    (status, body.to_vec())
}

async fn create_ledger(pool: &PgPool, label: &str) -> (Uuid, Uuid, Uuid) {
    let ledger_id = Uuid::now_v7();
    let wallet_id = Uuid::now_v7();
    let funding_id = Uuid::now_v7();

    sqlx::query("INSERT INTO ledger.ledgers (id, name) VALUES ($1, $2)")
        .bind(ledger_id)
        .bind(label)
        .execute(pool)
        .await
        .unwrap();

    create_ledger_account(
        pool,
        ledger_id,
        wallet_id,
        &AccountCode::new("asset:wallet").unwrap(),
        AccountType::Asset,
        "Wallet",
        false,
    )
    .await
    .unwrap();

    create_ledger_account(
        pool,
        ledger_id,
        funding_id,
        &AccountCode::new("asset:funding").unwrap(),
        AccountType::Asset,
        "Funding",
        true,
    )
    .await
    .unwrap();

    (ledger_id, wallet_id, funding_id)
}

async fn provision_key(pool: &PgPool, ledger_id: Uuid, fill: char, scopes: Vec<&str>) -> String {
    let token = format!("lk_test_{}", fill.to_string().repeat(43));
    let hash: [u8; 32] = Sha256::digest(token.as_bytes()).into();
    let key_id = Uuid::now_v7();

    sqlx::query(
        "INSERT INTO ledger.api_keys (id, key_hash, prefix, last4, scopes, label) \
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(key_id)
    .bind(hash.as_slice())
    .bind(&token[..16])
    .bind(&token[token.len() - 4..])
    .bind(scopes)
    .bind("HTTP integration key")
    .execute(pool)
    .await
    .unwrap();

    sqlx::query("INSERT INTO ledger.api_key_ledgers (api_key_id, ledger_id) VALUES ($1, $2)")
        .bind(key_id)
        .bind(ledger_id)
        .execute(pool)
        .await
        .unwrap();

    token
}

#[tokio::test]
async fn http_post_replays_bytes_and_enforces_tenant_scopes() {
    let container = Postgres::default().start().await.unwrap();
    let host = container.get_host().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let privileged_url = format!("postgres://postgres:postgres@{host}:{port}/postgres");
    let admin_pool = create_pool(&privileged_url).await.unwrap();

    sqlx::raw_sql(include_str!("../../../ops/postgres/bootstrap_roles.sql"))
        .execute(&admin_pool)
        .await
        .unwrap();
    run_migrations(&admin_pool).await.unwrap();

    let (ledger_id, wallet_id, funding_id) = create_ledger(&admin_pool, "Ledger A").await;
    let (foreign_id, _, _) = create_ledger(&admin_pool, "Ledger B").await;
    let key = provision_key(&admin_pool, ledger_id, 'A', vec!["read", "write"]).await;
    let read_key = provision_key(&admin_pool, ledger_id, 'B', vec!["read"]).await;

    // The HTTP pool uses a real non-superuser login inheriting ledger_app.
    sqlx::query("CREATE ROLE ledger_http_test LOGIN INHERIT PASSWORD 'test-only-password'")
        .execute(&admin_pool)
        .await
        .unwrap();
    sqlx::query("GRANT ledger_app TO ledger_http_test")
        .execute(&admin_pool)
        .await
        .unwrap();

    let runtime_url =
        format!("postgres://ledger_http_test:test-only-password@{host}:{port}/postgres");
    let runtime_pool = create_pool(&runtime_url).await.unwrap();
    assert_restricted_runtime_pool(&runtime_pool).await.unwrap();
    assert!(assert_restricted_runtime_pool(&admin_pool).await.is_err());

    // Simulate unsafe database permission changes.
    // Each case must be rejected by the runtime startup guard.
    let unsafe_grants = [
        (
            "GRANT SELECT ON public.accounts TO ledger_http_test",
            "REVOKE SELECT ON public.accounts FROM ledger_http_test",
        ),
        (
            "GRANT SELECT (label) ON ledger.api_keys TO ledger_http_test",
            "REVOKE SELECT (label) ON ledger.api_keys FROM ledger_http_test",
        ),
        (
            "GRANT UPDATE ON ledger.account_balances TO ledger_http_test",
            "REVOKE UPDATE ON ledger.account_balances FROM ledger_http_test",
        ),
        (
            "GRANT ledger_readonly TO ledger_http_test",
            "REVOKE ledger_readonly FROM ledger_http_test",
        ),
        (
            "GRANT ledger_migrator TO ledger_http_test",
            "REVOKE ledger_migrator FROM ledger_http_test",
        ),
        (
            "GRANT CREATE ON SCHEMA public TO ledger_http_test",
            "REVOKE CREATE ON SCHEMA public FROM ledger_http_test",
        ),
        (
            "GRANT EXECUTE ON FUNCTION \
             ledger.post_transaction( \
                 UUID, UUID, TEXT, TEXT, BYTEA, BYTEA, \
                 TIMESTAMPTZ, JSONB \
             ) TO ledger_http_test",
            "REVOKE EXECUTE ON FUNCTION \
             ledger.post_transaction( \
                 UUID, UUID, TEXT, TEXT, BYTEA, BYTEA, \
                 TIMESTAMPTZ, JSONB \
             ) FROM ledger_http_test",
        ),
    ];

    for (grant, revoke) in unsafe_grants {
        sqlx::query(grant).execute(&admin_pool).await.unwrap();

        let result = assert_restricted_runtime_pool(&runtime_pool).await;

        assert!(
            matches!(
                result,
                Err(kauri_ledger_store::StoreError::UnsafeRuntimeRole)
            ),
            "Unsafe permission was not rejected: {grant}"
        );

        // Restore the correct least-privilege configuration.
        sqlx::query(revoke).execute(&admin_pool).await.unwrap();

        assert_restricted_runtime_pool(&runtime_pool)
            .await
            .expect("Runtime must be accepted after removing the unsafe grant");
    }

    let app = build_router_with_ledger(LedgerApiState { pool: runtime_pool });
    let path = format!("/v1/ledgers/{ledger_id}/transactions");
    let body = format!(
        r#"{{"entries":[{{"account_id":"{wallet_id}","side":"D","amount":100}},{{"account_id":"{funding_id}","side":"C","amount":100}}]}}"#
    );

    let (status, initial) = send(
        &app,
        Method::POST,
        &path,
        Some(&key),
        Some("http-post-0001"),
        &body,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let posted: Value = serde_json::from_slice(&initial).unwrap();
    assert_eq!(posted["status"], "posted");
    assert!(posted["balances_after"].is_array());
    assert_eq!(posted["balances_after"].as_array().unwrap().len(), 2);
    // The response snapshot must match the actual committed balances.
    let balances = posted["balances_after"].as_array().unwrap();

    for (account_id, expected_total) in [(wallet_id, "100"), (funding_id, "-100")] {
        let account = serde_json::json!(account_id);

        let balance = balances
            .iter()
            .find(|item| item["account_id"] == account)
            .expect("Missing account in the posting snapshot");

        assert_eq!(balance["posted_total"], expected_total,);

        assert_eq!(balance["available_total"], expected_total,);

        assert_eq!(balance["reserved_total"], "0");
        assert_eq!(balance["version"], 1);
    }

    let equivalent = format!(
        r#"{{"entries":[{{"amount":100,"side":"D","account_id":"{wallet_id}"}},{{"amount":100,"side":"C","account_id":"{funding_id}"}}]}}"#
    );
    let (status, replay) = send(
        &app,
        Method::POST,
        &path,
        Some(&key),
        Some("http-post-0001"),
        &equivalent,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(replay, initial);

    let different = format!(
        r#"{{"entries":[{{"account_id":"{wallet_id}","side":"D","amount":101}},{{"account_id":"{funding_id}","side":"C","amount":101}}]}}"#
    );
    let (status, changed) = send(
        &app,
        Method::POST,
        &path,
        Some(&key),
        Some("http-post-0001"),
        &different,
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let changed: Value = serde_json::from_slice(&changed).unwrap();
    assert_eq!(changed["error"]["code"], "IDEMPOTENCY_KEY_REUSED");

    let balance_uri = format!("/v1/ledgers/{ledger_id}/accounts/asset:wallet/balance");
    let (status, balance) = send(&app, Method::GET, &balance_uri, Some(&key), None, "").await;
    assert_eq!(status, StatusCode::OK);
    let balance: Value = serde_json::from_slice(&balance).unwrap();
    assert_eq!(balance["posted_total"], "100");

    let foreign_uri = format!("/v1/ledgers/{foreign_id}/accounts/asset:wallet/balance");
    let (status, _) = send(&app, Method::GET, &foreign_uri, Some(&key), None, "").await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let foreign_post_path = format!("/v1/ledgers/{foreign_id}/transactions");
    let (status, _) = send(
        &app,
        Method::POST,
        &foreign_post_path,
        Some(&key),
        Some("foreign-post-001"),
        &body,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (status, _) = send(
        &app,
        Method::POST,
        &path,
        Some(&read_key),
        Some("http-post-0002"),
        &body,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let (status, _) = send(
        &app,
        Method::POST,
        &path,
        None,
        Some("http-post-0003"),
        &body,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let transaction_id = posted["transaction_id"].as_str().unwrap();
    let transaction_uri = format!("/v1/ledgers/{ledger_id}/transactions/{transaction_id}");
    let (status, transaction_body) =
        send(&app, Method::GET, &transaction_uri, Some(&key), None, "").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(transaction_body, initial);

    let duplicate_key_json = format!(
        r#"{{"entries":[],"entries":[{{"account_id":"{wallet_id}","side":"D","amount":100}}]}}"#
    );
    let (status, _) = send(
        &app,
        Method::POST,
        &path,
        Some(&key),
        Some("invalid-json-001"),
        &duplicate_key_json,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // C3: Reverse the committed transaction through HTTP.
    let reverse_uri = format!("{transaction_uri}/reverse");

    let (status, reversed_body) = send(
        &app,
        Method::POST,
        &reverse_uri,
        Some(&key),
        Some("http-reversal-0001"),
        "{}",
    )
    .await;

    assert_eq!(
        status,
        StatusCode::CREATED,
        "{}",
        String::from_utf8_lossy(&reversed_body),
    );

    let reversed: Value = serde_json::from_slice(&reversed_body).unwrap();

    assert_eq!(reversed["status"], "posted");
    assert_eq!(reversed["reverses_transaction"], transaction_id,);

    let reversal_id = reversed["transaction_id"].as_str().unwrap();

    assert_ne!(reversal_id, transaction_id);

    assert_eq!(reversed["balances_after"].as_array().unwrap().len(), 2,);

    // The exact same idempotent request returns stored bytes.
    let (status, replayed_reversal) = send(
        &app,
        Method::POST,
        &reverse_uri,
        Some(&key),
        Some("http-reversal-0001"),
        "{ }",
    )
    .await;

    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(replayed_reversal, reversed_body);

    // A second reversal with a different key is forbidden.
    let (status, duplicate_reversal) = send(
        &app,
        Method::POST,
        &reverse_uri,
        Some(&key),
        Some("http-reversal-0002"),
        "{}",
    )
    .await;

    assert_eq!(status, StatusCode::CONFLICT);

    let duplicate: Value = serde_json::from_slice(&duplicate_reversal).unwrap();

    assert_eq!(duplicate["error"]["code"], "ALREADY_REVERSED");

    // Only an empty JSON object is accepted for reversals.
    for invalid_body in [
        "[]",
        "null",
        "100",
        r#""reverse""#,
        r#"{"amount":100}"#,
        r#"{"effective_at":null}"#,
        r#"{"amount":100,"amount":200}"#,
    ] {
        let (status, error_body) = send(
            &app,
            Method::POST,
            &reverse_uri,
            Some(&key),
            Some("http-reversal-0003"),
            invalid_body,
        )
        .await;

        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "Invalid body was not rejected: {invalid_body}",
        );

        let error: Value = serde_json::from_slice(&error_body).unwrap();

        assert_eq!(error["error"]["code"], "VALIDATION_ERROR",);
    }

    // A read-only API key cannot reverse transactions.
    let (status, _) = send(
        &app,
        Method::POST,
        &reverse_uri,
        Some(&read_key),
        Some("http-reversal-0004"),
        "{}",
    )
    .await;

    assert_eq!(status, StatusCode::FORBIDDEN);

    // A missing API key cannot reverse transactions.
    let (status, _) = send(
        &app,
        Method::POST,
        &reverse_uri,
        None,
        Some("http-reversal-0005"),
        "{}",
    )
    .await;

    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // A key for ledger A cannot reverse an operation in ledger B.
    let foreign_reverse_uri =
        format!("/v1/ledgers/{foreign_id}/transactions/{transaction_id}/reverse");

    let (status, _) = send(
        &app,
        Method::POST,
        &foreign_reverse_uri,
        Some(&key),
        Some("http-reversal-0006"),
        "{}",
    )
    .await;

    assert_eq!(status, StatusCode::NOT_FOUND);

    // The original transaction remains readable and unchanged.
    let (status, original_after_reversal) =
        send(&app, Method::GET, &transaction_uri, Some(&key), None, "").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(original_after_reversal, initial);

    // The reversal itself can be read as a committed transaction.
    let reversal_transaction_uri = format!("/v1/ledgers/{ledger_id}/transactions/{reversal_id}");

    let (status, stored_reversal) = send(
        &app,
        Method::GET,
        &reversal_transaction_uri,
        Some(&key),
        None,
        "",
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(stored_reversal, reversed_body);

    // The wallet must return to its initial balance.
    let (status, restored_balance) =
        send(&app, Method::GET, &balance_uri, Some(&key), None, "").await;

    assert_eq!(status, StatusCode::OK);

    let restored: Value = serde_json::from_slice(&restored_balance).unwrap();

    assert_eq!(restored["posted_total"], "0");
    assert_eq!(restored["available_total"], "0");

    // Replaying the original posting must return its historical
    // response, even though the current wallet balance has changed.
    let (status, historical_replay) = send(
        &app,
        Method::POST,
        &path,
        Some(&key),
        Some("http-post-0001"),
        &body,
    )
    .await;

    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(historical_replay, initial);

    let (status, current_balance) =
        send(&app, Method::GET, &balance_uri, Some(&key), None, "").await;

    assert_eq!(status, StatusCode::OK);

    let current: Value = serde_json::from_slice(&current_balance).unwrap();

    assert_eq!(current["posted_total"], "0");

    // A reversal is also a transaction and may itself be reversed.
    let reverse_reversal_uri = format!("{reversal_transaction_uri}/reverse");

    let (status, second_reversal_body) = send(
        &app,
        Method::POST,
        &reverse_reversal_uri,
        Some(&key),
        Some("http-reversal-0007"),
        "{}",
    )
    .await;

    assert_eq!(
        status,
        StatusCode::CREATED,
        "{}",
        String::from_utf8_lossy(&second_reversal_body),
    );

    let second_reversal: Value = serde_json::from_slice(&second_reversal_body).unwrap();

    assert_eq!(second_reversal["reverses_transaction"], reversal_id,);

    // After two compensating reversals, the original
    // transaction's effect must be restored.
    let (status, final_balance) = send(&app, Method::GET, &balance_uri, Some(&key), None, "").await;

    assert_eq!(status, StatusCode::OK);

    let final_balance: Value = serde_json::from_slice(&final_balance).unwrap();

    assert_eq!(final_balance["posted_total"], "100");

    // Authorization must observe revocation immediately.
    sqlx::query("UPDATE ledger.api_keys SET revoked_at = NOW() WHERE key_hash = $1")
        .bind(Sha256::digest(key.as_bytes()).to_vec())
        .execute(&admin_pool)
        .await
        .unwrap();
    let (status, _) = send(&app, Method::GET, &balance_uri, Some(&key), None, "").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}
