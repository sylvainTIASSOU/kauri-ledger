use crate::amount::Amount;
use crate::entry::{Direction, Entry};
use crate::error::LedgerError;
use serde::{Deserialize, Deserializer, Serialize, de::Error as _};

/// Maximum amount alloawed for a single ledger entry.
pub const MAX_ENTRY_AMOUNT: i64 = 1_000_000_000_000;

/// Maximum number of entries allowed in a transaction.
pub const MAX_TRANSACTION_ENTRIES: usize = 64;

/// A transaction that is guaranteed to be balanced upon creation.
/// It is immutable and cannot be constructed if the invariants are violated.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BalancedTransaction {
    entries: Vec<Entry>,
}

impl BalancedTransaction {
    /// Attemps to crate a new BalancedTransaction.
    /// validations:
    /// 1. At least 2 entries.
    /// 2. All amounts > 0.
    /// 3. Total debits == Total credits.
    /// 4. No overflow during summation.
    pub fn new(entries: Vec<Entry>) -> Result<Self, LedgerError> {
        if entries.len() < 2 {
            return Err(LedgerError::InsufficientEntries);
        }

        if entries.len() > MAX_TRANSACTION_ENTRIES {
            return Err(LedgerError::TooManyEntries);
        }

        let mut total_debits = Amount::default();
        let mut total_credits = Amount::default();

        for entry in &entries {
            if entry.amount.is_zero() {
                return Err(LedgerError::InvalidAmount);
            }

            if entry.amount.as_i64() > MAX_ENTRY_AMOUNT {
                return Err(LedgerError::EntryAmountTooLarge);
            }

            match entry.direction {
                Direction::Debit => {
                    total_debits = total_debits.checked_add(entry.amount)?;
                }
                Direction::Credit => {
                    total_credits = total_credits.checked_add(entry.amount)?;
                }
            }
        }
        if total_debits != total_credits {
            return Err(LedgerError::TransactionNotBalanced);
        }
        Ok(Self { entries })
    }

    /// Returns a reference to the entries.
    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UnvalidatedTransaction {
    entries: Vec<Entry>,
}

impl<'de> Deserialize<'de> for BalancedTransaction {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = UnvalidatedTransaction::deserialize(deserializer)?;

        Self::new(raw.entries).map_err(D::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::AccountCode;
    use crate::balance::calculate_balance_delta;
    use proptest::prelude::*;

    // Property 1 & 2: Balance and Conservation (Sum of all deltas must be 0)
    proptest! {
        #[test]
        fn test_transaction_balance_and_conservation(
            amount in 1i64..1_000_000_000i64,
        ) {
            let amt = Amount::new(amount).unwrap();
            let entries = vec![
                Entry::new(AccountCode::new("asset:cash").unwrap(), Direction::Debit, amt),
                Entry::new(AccountCode::new("liability:debt").unwrap(), Direction::Credit, amt),
            ];

            let tx = BalancedTransaction::new(entries).unwrap();

            let deltas = calculate_balance_delta(&tx).unwrap();
            let sum: i64 = deltas.values().sum();

            // The sum of all deltas in a balanced transaction must be exactly 0.
            prop_assert_eq!(sum, 0);
        }

        #[test]
        fn test_unbalanced_transaction_rejected(
            debit_amount in 1i64..1_000_000_000i64,
            credit_amount in 1i64..1_000_000_000i64,
        ) {
            // Ensure they are different
            let d_amt = debit_amount;
            let c_amt = if debit_amount == credit_amount { debit_amount + 1 } else { credit_amount };

            let d_amount = Amount::new(d_amt.max(1)).unwrap();
            let c_amount = Amount::new(c_amt.max(1)).unwrap();

            let entries = vec![
                Entry::new(AccountCode::new("asset:cash").unwrap(), Direction::Debit, d_amount),
                Entry::new(AccountCode::new("liability:debt").unwrap(), Direction::Credit, c_amount),
            ];

            let tx = BalancedTransaction::new(entries);
            prop_assert!(tx.is_err());
            prop_assert_eq!(tx.unwrap_err(), LedgerError::TransactionNotBalanced);
        }

        // Property 7: No overflow
        #[test]
        fn test_no_overflow_property(
            amount1 in 1i64..i64::MAX,
            amount2 in 1i64..i64::MAX,
        ) {
            let a1 = Amount::new(amount1).unwrap_or_else(|_| Amount::new(i64::MAX).unwrap());
            let a2 = Amount::new(amount2).unwrap_or_else(|_| Amount::new(i64::MAX).unwrap());

            let res = a1.checked_add(a2);
            if amount1 > i64::MAX - amount2 {
                prop_assert_eq!(res, Err(LedgerError::Overflow));
            } else {
                prop_assert!(res.is_ok());
            }
        }
    }

    // Property 6: Reversibility
    #[test]
    fn test_reversibility() {
        let amount = Amount::new(5000).unwrap();
        let entries = vec![
            Entry::new(
                AccountCode::new("wallet:client").unwrap(),
                Direction::Debit,
                amount,
            ),
            Entry::new(
                AccountCode::new("revenue:fees").unwrap(),
                Direction::Credit,
                amount,
            ),
        ];
        let tx = BalancedTransaction::new(entries).unwrap();

        // Reverse the transaction
        let reversed_entries: Vec<Entry> = tx
            .entries()
            .iter()
            .map(|e| {
                let rev_dir = match e.direction {
                    Direction::Debit => Direction::Credit,
                    Direction::Credit => Direction::Debit,
                };
                Entry::new(e.account_code.clone(), rev_dir, e.amount)
            })
            .collect();

        let reversed_tx = BalancedTransaction::new(reversed_entries).unwrap();

        // Calculate deltas
        let delta1 = calculate_balance_delta(&tx).unwrap();
        let delta2 = calculate_balance_delta(&reversed_tx).unwrap();

        // Sum of deltas should be zero for all accounts
        let mut combined = delta1;
        for (acc, delta) in delta2 {
            *combined.entry(acc).or_insert(0) += delta;
        }

        for (_, net_delta) in combined {
            assert_eq!(net_delta, 0);
        }
    }
}
