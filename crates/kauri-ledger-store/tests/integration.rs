mod common;

use kauri_ledger_core::{AccountCode, AccountType, Amount, BalancedTransaction, Direction, Entry};
use kauri_ledger_store::{
    StoreError, create_account, get_account_history, get_balance, record_transaction,
};

#[tokio::test]
async fn test_create_account_and_record_transaction() {
    let db = common::TestDb::new().await;

    // Create two accounts
    let asset_code = AccountCode::new("asset:cash").unwrap();
    let liability_code = AccountCode::new("liability:debt").unwrap();

    create_account(&db.pool, &asset_code, AccountType::Asset)
        .await
        .expect("Failed to create asset account");
    create_account(&db.pool, &liability_code, AccountType::Liability)
        .await
        .expect("Failed to create liability account");

    // Create a balanced transaction
    let amount = Amount::new(5000).unwrap();
    let entries = vec![
        Entry::new(asset_code.clone(), Direction::Debit, amount),
        Entry::new(liability_code.clone(), Direction::Credit, amount),
    ];
    let tx = BalancedTransaction::new(entries).unwrap();

    // Record the transaction
    let idempotency_key = "test-key-1";
    let tx_id = record_transaction(&db.pool, idempotency_key, &tx)
        .await
        .expect("Failed to record transaction");

    assert!(!tx_id.is_nil());

    // Verify balances
    let asset_balance = get_balance(&db.pool, &asset_code)
        .await
        .expect("Failed to get asset balance")
        .expect("Asset balance not found");
    assert_eq!(asset_balance.balance, 5000);

    let liability_balance = get_balance(&db.pool, &liability_code)
        .await
        .expect("Failed to get liability balance")
        .expect("Liability balance not found");
    assert_eq!(liability_balance.balance, -5000); // Credit is negative delta

    // Verify history
    let asset_history = get_account_history(&db.pool, &asset_code)
        .await
        .expect("Failed to get asset history");
    assert_eq!(asset_history.len(), 1);
    assert_eq!(asset_history[0].amount, 5000);
    assert_eq!(asset_history[0].direction, "Debit");
}

#[tokio::test]
async fn test_idempotence_same_key_same_payload() {
    let db = common::TestDb::new().await;

    let asset_code = AccountCode::new("asset:cash").unwrap();
    let liability_code = AccountCode::new("liability:debt").unwrap();

    create_account(&db.pool, &asset_code, AccountType::Asset)
        .await
        .unwrap();
    create_account(&db.pool, &liability_code, AccountType::Liability)
        .await
        .unwrap();

    let amount = Amount::new(3000).unwrap();
    let entries = vec![
        Entry::new(asset_code.clone(), Direction::Debit, amount),
        Entry::new(liability_code.clone(), Direction::Credit, amount),
    ];
    let tx = BalancedTransaction::new(entries).unwrap();

    let idempotency_key = "idempotent-key-1";

    // Record the transaction twice with the same key and payload
    let tx_id_1 = record_transaction(&db.pool, idempotency_key, &tx)
        .await
        .unwrap();
    let tx_id_2 = record_transaction(&db.pool, idempotency_key, &tx)
        .await
        .unwrap();

    // Should return the same transaction ID
    assert_eq!(tx_id_1, tx_id_2);

    // Balance should only reflect one transaction
    let asset_balance = get_balance(&db.pool, &asset_code).await.unwrap().unwrap();
    assert_eq!(asset_balance.balance, 3000);
}

#[tokio::test]
async fn test_idempotence_conflict_same_key_different_payload() {
    let db = common::TestDb::new().await;

    let asset_code = AccountCode::new("asset:cash").unwrap();
    let liability_code = AccountCode::new("liability:debt").unwrap();

    create_account(&db.pool, &asset_code, AccountType::Asset)
        .await
        .unwrap();
    create_account(&db.pool, &liability_code, AccountType::Liability)
        .await
        .unwrap();

    // First transaction
    let amount1 = Amount::new(3000).unwrap();
    let entries1 = vec![
        Entry::new(asset_code.clone(), Direction::Debit, amount1),
        Entry::new(liability_code.clone(), Direction::Credit, amount1),
    ];
    let tx1 = BalancedTransaction::new(entries1).unwrap();

    let idempotency_key = "conflict-key-1";
    record_transaction(&db.pool, idempotency_key, &tx1)
        .await
        .unwrap();

    // Second transaction with same key but different amount
    let amount2 = Amount::new(5000).unwrap();
    let entries2 = vec![
        Entry::new(asset_code.clone(), Direction::Debit, amount2),
        Entry::new(liability_code.clone(), Direction::Credit, amount2),
    ];
    let tx2 = BalancedTransaction::new(entries2).unwrap();

    // Should fail with IdempotencyConflict
    let result = record_transaction(&db.pool, idempotency_key, &tx2).await;
    assert!(matches!(result, Err(StoreError::IdempotencyConflict)));
}

#[tokio::test]
async fn test_unbalanced_transaction_rejected() {
    let db = common::TestDb::new().await;

    let asset_code = AccountCode::new("asset:cash").unwrap();
    let liability_code = AccountCode::new("liability:debt").unwrap();

    create_account(&db.pool, &asset_code, AccountType::Asset)
        .await
        .unwrap();
    create_account(&db.pool, &liability_code, AccountType::Liability)
        .await
        .unwrap();

    // Try to create an unbalanced transaction (should fail at domain level)
    let amount1 = Amount::new(3000).unwrap();
    let amount2 = Amount::new(5000).unwrap();
    let entries = vec![
        Entry::new(asset_code.clone(), Direction::Debit, amount1),
        Entry::new(liability_code.clone(), Direction::Credit, amount2),
    ];

    let result = BalancedTransaction::new(entries);
    assert!(result.is_err());
}

#[tokio::test]
async fn test_immutability_trigger_prevents_entry_modification() {
    let db = common::TestDb::new().await;

    let asset_code = AccountCode::new("asset:cash").unwrap();
    let liability_code = AccountCode::new("liability:debt").unwrap();

    create_account(&db.pool, &asset_code, AccountType::Asset)
        .await
        .unwrap();
    create_account(&db.pool, &liability_code, AccountType::Liability)
        .await
        .unwrap();

    let amount = Amount::new(1000).unwrap();
    let entries = vec![
        Entry::new(asset_code.clone(), Direction::Debit, amount),
        Entry::new(liability_code.clone(), Direction::Credit, amount),
    ];
    let tx = BalancedTransaction::new(entries).unwrap();

    record_transaction(&db.pool, "immutability-test", &tx)
        .await
        .unwrap();

    // Try to update an entry directly (should fail due to trigger)
    let result: Result<(), sqlx::Error> =
        sqlx::query("UPDATE entries SET amount = 9999 WHERE account_code = $1")
            .bind(asset_code.as_str())
            .execute(&db.pool)
            .await
            .map(|_| ());

    assert!(result.is_err());
    if let Err(sqlx::Error::Database(db_err)) = result {
        assert!(db_err.message().contains("ENTRIES_IMMUTABLE"));
    }
}
