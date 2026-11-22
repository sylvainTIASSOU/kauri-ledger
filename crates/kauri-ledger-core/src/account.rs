use crate::error::LedgerError;
use serde::{Deserialize, Deserializer, Serialize, de::Error as _};

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
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub struct AccountCode(String);

impl AccountCode {
    /// Creates a new AccountCode. Returns an error if the code is empty.
    pub fn new(code: impl Into<String>) -> Result<Self, LedgerError> {
        let code = code.into();
        let bytes = code.as_bytes();
        let valid = bytes.len() <= 128
            && bytes
                .first()
                .is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
            && bytes.iter().skip(1).all(|byte| {
                byte.is_ascii_lowercase()
                    || byte.is_ascii_digit()
                    || matches!(*byte, b':' | b'_' | b'-')
            });

        if !valid {
            return Err(LedgerError::InvalidAccountCode);
        }

        Ok(Self(code))
    }

    /// Returns the inner string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for AccountCode {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let code = String::deserialize(deserializer)?;

        Self::new(code).map_err(D::Error::custom)
    }
}
