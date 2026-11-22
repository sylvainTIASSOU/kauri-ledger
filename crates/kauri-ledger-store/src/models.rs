use chrono::{DateTime, Utc};
use uuid::Uuid;

/// Represents the type of an account in the database.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::Type)]
#[sqlx(type_name = "VARCHAR", rename_all = "PascalCase")]
pub enum DbAccountType {
    Asset,
    Liability,
    Equity,
    Revenue,
    Expense,
}

/// Database representation of an account.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct DbAccount {
    pub code: String,
    pub account_type: DbAccountType,
    pub created_at: DateTime<Utc>,
}

/// Database representation of a transaction.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct DbTransaction {
    pub id: Uuid,
    pub idempotency_key: String,
    pub payload_hash: String,
    pub created_at: DateTime<Utc>,
}

/// Database representation of a ledger entry.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct DbEntry {
    pub id: Uuid,
    pub transaction_id: Uuid,
    pub account_code: String,
    pub direction: String,
    pub amount: i64,
    pub created_at: DateTime<Utc>,
}

/// Database representation of an account balance.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct DbBalance {
    pub account_code: String,
    pub balance: i64,
    pub version: i64,
    pub updated_at: DateTime<Utc>,
}
