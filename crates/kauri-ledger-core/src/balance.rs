use crate::account::AccountCode;
use crate::entry::Direction;
use crate::transaction::BalancedTransaction;
use std::collections::HashMap;

/// Calculates the net effect of a transaction on account balances.
/// Returns a map of AccountCode to the net change.
/// Debit is represented as a positive delta, Credit as a negative delta.
/// (The interpretation of this delta depends on the AccountType, handled at the store level).
pub fn calculate_balance_delta(tx: &BalancedTransaction) -> HashMap<AccountCode, i64> {
    let mut deltas: HashMap<AccountCode, i64> = HashMap::new();

    for entry in tx.entries() {
        let delta = match entry.direction {
            Direction::Debit => entry.amount.as_i64(),
            Direction::Credit => -entry.amount.as_i64(),
        };
        *deltas.entry(entry.account_code.clone()).or_insert(0) += delta;
    }
    deltas
}
