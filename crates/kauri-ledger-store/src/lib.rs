//! PostgreSQL persistence layer for Kauri Ledger.

pub mod db;
pub mod error;
pub mod models;
pub mod repository;
pub mod tenant_repository;

pub use db::{create_pool, run_migrations};
pub use error::StoreError;
pub use models::{DbAccount, DbBalance, DbEntry, DbTransaction};
pub use repository::{
    calculate_actual_balance, create_account, get_account_history, get_balance, record_transaction,
    verify_account_balance_integrity,
};
