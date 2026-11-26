use kauri_ledger_core::{
    AccountCode, Amount, BalancedTransaction, Direction, Entry, calculate_balance_delta,
};

fn sample_transaction() -> BalancedTransaction {
    BalancedTransaction::new(vec![
        Entry::new(
            AccountCode::new("asset:cash").unwrap(),
            Direction::Debit,
            Amount::new(10_000).unwrap(),
        ),
        Entry::new(
            AccountCode::new("liability:seller").unwrap(),
            Direction::Credit,
            Amount::new(8_500).unwrap(),
        ),
        Entry::new(
            AccountCode::new("revenue:fees").unwrap(),
            Direction::Credit,
            Amount::new(1_500).unwrap(),
        ),
    ])
    .unwrap()
}

#[test]
fn reversal_preserves_accounts_amounts_and_entry_order() {
    let original = sample_transaction();

    let reversal = original.reversed().unwrap();

    assert_eq!(original.entries().len(), reversal.entries().len());

    for (original_entry, reversed_entry) in original.entries().iter().zip(reversal.entries()) {
        assert_eq!(original_entry.account_code, reversed_entry.account_code);

        assert_eq!(original_entry.amount, reversed_entry.amount);

        assert_ne!(original_entry.direction, reversed_entry.direction);
    }

    // The original transaction must remain unchanged.
    assert_eq!(original.entries()[0].direction, Direction::Debit);
}

#[test]
fn reversal_cancels_every_account_delta() {
    let original = sample_transaction();
    let reversal = original.reversed().unwrap();

    let original_deltas = calculate_balance_delta(&original).unwrap();

    let reversal_deltas = calculate_balance_delta(&reversal).unwrap();

    assert_eq!(original_deltas.len(), reversal_deltas.len());

    for (account_code, original_delta) in original_deltas {
        assert_eq!(
            reversal_deltas.get(&account_code).copied(),
            Some(-original_delta)
        );
    }
}

#[test]
fn reversing_a_reversal_restores_the_original_entries() {
    let original = sample_transaction();

    let first_reversal = original.reversed().unwrap();

    let second_reversal = first_reversal.reversed().unwrap();

    assert_eq!(second_reversal, original);
}
