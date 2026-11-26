use crate::account::AccountCode;
use crate::entry::Direction;
use crate::error::LedgerError;
use crate::transaction::BalancedTransaction;
use std::collections::HashMap;

/// Calculates the net effect of a transaction on account balances.
/// Returns a map of AccountCode to the net change.
/// Debit is represented as a positive delta, Credit as a negative delta.
/// (The interpretation of this delta depends on the AccountType, handled at the store level).
pub fn calculate_balance_delta(
    tx: &BalancedTransaction,
) -> Result<HashMap<AccountCode, i64>, LedgerError> {
    let mut deltas: HashMap<AccountCode, i64> = HashMap::new();

    for entry in tx.entries() {
        let amount = entry.amount.as_i64();

        let delta = match entry.direction {
            Direction::Debit => amount,
            Direction::Credit => amount.checked_neg().ok_or(LedgerError::Overflow)?,
        };

        let account_delta = deltas.entry(entry.account_code.clone()).or_insert(0);

        *account_delta = account_delta
            .checked_add(delta)
            .ok_or(LedgerError::Overflow)?;
    }

    Ok(deltas)
}
