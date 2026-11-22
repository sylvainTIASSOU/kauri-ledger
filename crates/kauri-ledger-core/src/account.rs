use crate::error::LedgerError;
use serde::{Deserialize, Serialize};

/// The type of an account, following standard accounting principles.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Copy)]
pub enum AccountType {
    Asset,
    Liability,
    Equity,
    Revenue,
    Expense,
}

/// A unique, human-readable identifier for an account (e.g., "wallet:client:ama").
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AccountCode(String);

impl AccountCode {
    /// Creates a new AccountCode. Returns an error if the code is empty.
    pub fn new(code: impl Into<String>) -> Result<Self, LedgerError> {
        let code = code.into();
        if code.trim().is_empty() {
            Err(LedgerError::InvalidAccountCode)
        } else {
            Ok(Self(code))
        }
    }

    /// Returns the inner string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
