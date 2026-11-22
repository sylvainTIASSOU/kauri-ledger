use kauri_ledger_core::{
    AccountCode, Amount, BalancedTransaction, Direction, Entry, LedgerError, MAX_ENTRY_AMOUNT,
    MAX_TRANSACTION_ENTRIES, calculate_balance_delta,
};

fn balanced_entries(pair_count: usize, amount: Amount) -> Vec<Entry> {
    let debit_account = AccountCode::new("asset:cash").unwrap();
    let credit_account = AccountCode::new("liability:debt").unwrap();

    let mut entries = Vec::with_capacity(pair_count * 2);

    for _ in 0..pair_count {
        entries.push(Entry::new(debit_account.clone(), Direction::Debit, amount));

        entries.push(Entry::new(
            credit_account.clone(),
            Direction::Credit,
            amount,
        ));
    }

    entries
}

#[test]
fn accepts_maximum_number_of_entries() {
    let entries = balanced_entries(32, Amount::new(1).unwrap());

    assert_eq!(entries.len(), MAX_TRANSACTION_ENTRIES);
    assert!(BalancedTransaction::new(entries).is_ok());
}

#[test]
fn rejects_more_than_maximum_number_of_entries() {
    let mut entries = balanced_entries(32, Amount::new(1).unwrap());

    entries.push(Entry::new(
        AccountCode::new("asset:cash").unwrap(),
        Direction::Debit,
        Amount::new(1).unwrap(),
    ));

    entries[1].amount = Amount::new(2).unwrap();

    assert_eq!(entries.len(), MAX_TRANSACTION_ENTRIES + 1);

    assert_eq!(
        BalancedTransaction::new(entries),
        Err(LedgerError::TooManyEntries)
    );
}

#[test]
fn accepts_maximum_entry_amount() {
    let amount = Amount::new(MAX_ENTRY_AMOUNT).unwrap();
    let entries = balanced_entries(1, amount);

    assert!(BalancedTransaction::new(entries).is_ok());
}

#[test]
fn rejects_entry_amount_above_limit() {
    let amount = Amount::new(MAX_ENTRY_AMOUNT + 1).unwrap();
    let entries = balanced_entries(1, amount);

    assert_eq!(
        BalancedTransaction::new(entries),
        Err(LedgerError::EntryAmountTooLarge)
    );
}

#[test]
fn subtraction_cannot_produce_negative_amount() {
    let smaller = Amount::new(3).unwrap();
    let larger = Amount::new(5).unwrap();

    assert_eq!(
        smaller.checked_sub(larger),
        Err(LedgerError::AmountUnderflow)
    );
}

#[test]
fn subtraction_allows_zero_result() {
    let amount = Amount::new(5).unwrap();

    let result = amount.checked_sub(amount).unwrap();

    assert_eq!(result.as_i64(), 0);
}

#[test]
fn balance_deltas_preserve_conservation() {
    let debit_account = AccountCode::new("asset:cash").unwrap();
    let liability_account = AccountCode::new("liability:debt").unwrap();
    let revenue_account = AccountCode::new("revenue:fees").unwrap();

    let entries = vec![
        Entry::new(
            debit_account.clone(),
            Direction::Debit,
            Amount::new(100).unwrap(),
        ),
        Entry::new(
            liability_account.clone(),
            Direction::Credit,
            Amount::new(80).unwrap(),
        ),
        Entry::new(
            revenue_account.clone(),
            Direction::Credit,
            Amount::new(20).unwrap(),
        ),
    ];

    let tx = BalancedTransaction::new(entries).unwrap();

    let deltas = calculate_balance_delta(&tx).unwrap();

    assert_eq!(deltas.get(&debit_account), Some(&100));
    assert_eq!(deltas.get(&liability_account), Some(&-80));
    assert_eq!(deltas.get(&revenue_account), Some(&-20));

    assert_eq!(deltas.values().sum::<i64>(), 0);
}

#[test]
fn checked_add_detects_overflow() {
    let maximum = Amount::new(i64::MAX).unwrap();
    let one = Amount::new(1).unwrap();

    assert_eq!(maximum.checked_add(one), Err(LedgerError::Overflow));
}
