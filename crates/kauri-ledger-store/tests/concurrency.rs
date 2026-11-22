mod common;

use futures::future::join_all;
use kauri_ledger_core::{AccountCode, AccountType, Amount, BalancedTransaction, Direction, Entry};
use kauri_ledger_store::{StoreError, create_account, get_balance, record_transaction};
use std::collections::HashSet;
use uuid::Uuid;

/// Property 4: The same idempotency key sent 50 times in parallel produces exactly 1 transaction.
#[tokio::test]
async fn test_idempotence_under_high_concurrency() {
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

    let idempotency_key = "super-parallel-key-1";
    let num_requests = 50;

    // Spawn 50 concurrent tasks with the EXACT same key and payload
    let mut handles = Vec::new();
    for _ in 0..num_requests {
        let pool = db.pool.clone();
        let tx_clone = tx.clone();
        let key = idempotency_key.to_string();

        let handle = tokio::spawn(async move { record_transaction(&pool, &key, &tx_clone).await });
        handles.push(handle);
    }

    let results = join_all(handles).await;

    // Extract successful results
    let mut tx_ids: HashSet<Uuid> = HashSet::new();
    for result in results {
        let tx_id = result
            .expect("Task panicked")
            .expect("Transaction should succeed or return existing ID");
        tx_ids.insert(tx_id);
    }

    // Property 4: Exactly 1 unique transaction ID should exist
    assert_eq!(
        tx_ids.len(),
        1,
        "Expected exactly 1 unique transaction ID, got {}",
        tx_ids.len()
    );

    // Verify balance reflects only ONE transaction, not 50
    let asset_balance = get_balance(&db.pool, &asset_code).await.unwrap().unwrap();
    assert_eq!(
        asset_balance.balance, 1000,
        "Balance should be 1000, not {}",
        asset_balance.balance
    );
}

/// Properties 2, 3, 5: 1,000 parallel transactions on a few accounts.
/// Verifies: No deadlocks (Property 5), Conservation of funds (Property 2), No negative balances if not intended (Property 3).
#[tokio::test]
async fn test_high_concurrency_no_deadlocks_and_conservation() {
    let db = common::TestDb::new().await;

    let account_a = AccountCode::new("asset:account_a").unwrap();
    let account_b = AccountCode::new("asset:account_b").unwrap();

    create_account(&db.pool, &account_a, AccountType::Asset)
        .await
        .unwrap();
    create_account(&db.pool, &account_b, AccountType::Asset)
        .await
        .unwrap();

    let num_transactions = 1000;
    let amount = Amount::new(100).unwrap();
    let mut handles = Vec::new();

    for i in 0..num_transactions {
        let pool = db.pool.clone();
        let acc_a = account_a.clone();
        let acc_b = account_b.clone();
        let amt = amount;
        let idempotency_key = format!("concurrent-tx-{}", i);

        let handle = tokio::spawn(async move {
            // Alternate directions to maximize lock contention on both accounts
            let entries = if i % 2 == 0 {
                vec![
                    Entry::new(acc_a, Direction::Debit, amt),
                    Entry::new(acc_b, Direction::Credit, amt),
                ]
            } else {
                vec![
                    Entry::new(acc_b, Direction::Debit, amt),
                    Entry::new(acc_a, Direction::Credit, amt),
                ]
            };

            let tx = BalancedTransaction::new(entries).unwrap();
            record_transaction(&pool, &idempotency_key, &tx).await
        });
        handles.push(handle);
    }

    let results = join_all(handles).await;

    let mut success_count = 0;
    let mut deadlock_count = 0;

    for result in results {
        match result.expect("Task panicked") {
            Ok(_) => success_count += 1,
            Err(StoreError::MaxRetriesExceeded(_)) => deadlock_count += 1,
            Err(e) => panic!("Unexpected error: {:?}", e),
        }
    }

    // Property 5: Zero deadlocks tolerated (ordered locking must prevent them)
    assert_eq!(
        deadlock_count, 0,
        "Expected 0 deadlocks, but got {}",
        deadlock_count
    );

    // All 1000 unique transactions should succeed
    assert_eq!(
        success_count, num_transactions,
        "Expected {} successes, got {}",
        num_transactions, success_count
    );

    // Property 2: Conservation of funds.
    // Since we did 500 A->B and 500 B->A of the same amount, net delta for both must be 0.
    let balance_a = get_balance(&db.pool, &account_a).await.unwrap().unwrap();
    let balance_b = get_balance(&db.pool, &account_b).await.unwrap().unwrap();

    assert_eq!(
        balance_a.balance, 0,
        "Account A balance should be 0 due to conservation, got {}",
        balance_a.balance
    );
    assert_eq!(
        balance_b.balance, 0,
        "Account B balance should be 0 due to conservation, got {}",
        balance_b.balance
    );
}
