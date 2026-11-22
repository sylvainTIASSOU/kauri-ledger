use thiserror::Error;

/// Errors specific to the storage layer.
#[derive(Error, Debug)]
pub enum StoreError {
    #[error("DATABASE_ERROR: {0}")]
    Database(#[from] sqlx::Error),

    #[error("IDEMPOTENCY_CONFLICT: Idempotent key already used with a different payload")]
    IdempotencyConflict,

    #[error("ACCOUNT_NOT_FOUND: Account with code {0} does not exist")]
    AccountNotFound(String),

    #[error(
        "MAX_RETRIES_EXCEEDED: Could not complete operation after {0} retries due to deedlocks"
    )]
    MaxRetriesExceeded(u32),

    #[error("MIGRATION_ERROR: {0}")]
    MigrationError(#[from] sqlx::migrate::MigrateError),

    #[error(
        "BALANCE_INTEGRITY_ERROR: Account '{account_code}' has stored balance {stored} but actual calculated balance is {actual}"
    )]
    BalanceIntegrityError {
        account_code: String,
        stored: i64,
        actual: i64,
    },
}

/// Checks if a sqlx::Error is a PostgreSql deadlock (SQLSTATE 40P01)
pub fn is_deadlock_error(err: &sqlx::Error) -> bool {
    if let sqlx::Error::Database(db_err) = err {
        db_err.code().as_deref() == Some("40P01")
    } else {
        false
    }
}

/// Check if sqlx::Error is a unique constraint violation (SQLSTATE 23505)
pub fn is_unique_violation(err: &sqlx::Error) -> bool {
    if let sqlx::Error::Database(db_err) = err {
        db_err.code().as_deref() == Some("23505")
    } else {
        false
    }
}
