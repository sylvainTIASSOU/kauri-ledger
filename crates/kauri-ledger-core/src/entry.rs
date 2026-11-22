use crate::account::AccountCode;
use crate::amount::Amount;
use serde::{Deserialize, Serialize};

/// The direction of an entry in the ledger.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Direction {
    Debit,
    Credit,
}

/// A single line item in a transaction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub account_code: AccountCode,
    pub amount: Amount,
    pub direction: Direction,
}

impl Entry {
    pub fn new(account_code: AccountCode, direction: Direction, amount: Amount) -> Self {
        Self {
            account_code,
            amount,
            direction,
        }
    }
}
