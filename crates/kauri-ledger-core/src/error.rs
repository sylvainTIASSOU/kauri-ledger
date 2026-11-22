use thiserror::Error;

/// Stable error codes for buisiness logic failures.
/// These codes are guaranteed not to change between versions.

#[derive(Debug, Error, PartialEq, Eq)]
pub enum LedgerError {
    #[error("TRANSACTION_NOT_BALANCED: Total debits must equal total credits")]
    TransactionNotBalanced,

    #[error("INVALID_AMOUNT: Amount must be strictly greater than zero")]
    InvalidAmount,

    #[error("INSUFFICIENT_ENTRIES: A transaction must have at least two entries")]
    InsufficientEntries,

    #[error("OVERFLOW: Arithmetic operation resulted in an overflow")]
    Overflow,

    #[error("INVALID_ACCOUNT_CODE: Account code does not match the required format")]
    InvalidAccountCode,
}
