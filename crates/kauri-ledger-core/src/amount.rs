use crate::error::LedgerError;

use serde::{Deserialize, Serialize};

/// Represents a monetary amount in whole XOF currency units (Francs CFA).
/// No floating-point numbers are used to avoid rounding errors.

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, Default)]
pub struct Amount(i64);

impl Amount {
    /// Creates a new Amount. Return an error if the amount is negative.
    pub fn new(amount: i64) -> Result<Self, LedgerError> {
        if amount < 0 {
            return Err(LedgerError::InvalidAmount);
        }
        Ok(Self(amount))
    }

    /// Returns the inner i64 value.
    pub fn as_i64(&self) -> i64 {
        self.0
    }

    /// Adds two amounts with overflow checking.
    pub fn checked_add(self, other: Self) -> Result<Self, LedgerError> {
        self.0
            .checked_add(other.0)
            .map(Amount)
            .ok_or(LedgerError::Overflow)
    }

    /// Subtracts two amounts with overflow checking.
    pub fn checked_sub(self, other: Self) -> Result<Self, LedgerError> {
        self.0
            .checked_sub(other.0)
            .map(Self)
            .ok_or(LedgerError::Overflow)
    }

    /// Returns true if the amount is zero.
    pub fn is_zero(&self) -> bool {
        self.0 == 0
    }
}
