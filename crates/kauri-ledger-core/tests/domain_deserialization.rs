use kauri_ledger_core::{AccountCode, Amount, BalancedTransaction, Direction, Entry};
use serde_json::json;

#[test]
fn rejects_invalid_account_codes() {
    let invalid_codes = ["", "Asset:cash", " asset:cash", "asset/cash", "épargne"];

    for code in invalid_codes {
        assert!(AccountCode::new(code).is_err());
        assert!(serde_json::from_value::<AccountCode>(json!(code)).is_err());
    }

    assert!(AccountCode::new("a".repeat(129)).is_err());
}

#[test]
fn accepts_valid_account_codes() {
    let code: AccountCode = serde_json::from_value(json!("wallet:client_1")).unwrap();

    assert_eq!(code.as_str(), "wallet:client_1");
}

#[test]
fn rejects_negative_amounts_from_json() {
    assert!(serde_json::from_str::<Amount>("-1").is_err());

    let zero: Amount = serde_json::from_str("0").unwrap();
    assert_eq!(zero.as_i64(), 0);
}

#[test]
fn rejects_empty_transactions_from_json() {
    let value = json!({
        "entries": []
    });

    assert!(serde_json::from_value::<BalancedTransaction>(value).is_err());
}

#[test]
fn rejects_unbalanced_transactions_from_json() {
    let value = json!({
        "entries": [
            {
                "account_code": "asset:cash",
                "direction": "Debit",
                "amount": 100
            },
            {
                "account_code": "liability:debt",
                "direction": "Credit",
                "amount": 99
            }
        ]
    });

    assert!(serde_json::from_value::<BalancedTransaction>(value).is_err());
}

#[test]
fn rejects_zero_value_entries_from_json() {
    let value = json!({
        "entries": [
            {
                "account_code": "asset:cash",
                "direction": "Debit",
                "amount": 0
            },
            {
                "account_code": "liability:debt",
                "direction": "Credit",
                "amount": 0
            }
        ]
    });

    assert!(serde_json::from_value::<BalancedTransaction>(value).is_err());
}

#[test]
fn valid_transaction_survives_serialization_round_trip() {
    let amount = Amount::new(5_000).unwrap();

    let entries = vec![
        Entry::new(
            AccountCode::new("asset:cash").unwrap(),
            Direction::Debit,
            amount,
        ),
        Entry::new(
            AccountCode::new("liability:debt").unwrap(),
            Direction::Credit,
            amount,
        ),
    ];

    let original = BalancedTransaction::new(entries).unwrap();

    let serialized = serde_json::to_string(&original).unwrap();
    let restored: BalancedTransaction = serde_json::from_str(&serialized).unwrap();

    assert_eq!(original, restored);
}
