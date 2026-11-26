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

    #[error("TOO_MANY_ENTRIES: A transaction cannot exceed 64 entries")]
    TooManyEntries,

    #[error("ENTRY_AMOUNT_TOO_LARGE: Entry amount exceeds the maximum allowed value")]
    EntryAmountTooLarge,

    #[error("AMOUNT_UNDERFLOW: Subtraction would produce a negative amount")]
    AmountUnderflow,

    #[error("OVERFLOW: Arithmetic operation resulted in an overflow")]
    Overflow,

    #[error("INVALID_ACCOUNT_CODE: Account code does not match the required format")]
    InvalidAccountCode,
}
