//! Core domain logic for the Kauri Ledger.
//! No I/O, no database, no network dependencies.
#![forbid(unsafe_code)]
pub mod account;
pub mod amount;
pub mod balance;
pub mod entry;
pub mod error;
pub mod transaction;

pub use account::{AccountCode, AccountType};
pub use amount::Amount;
pub use balance::calculate_balance_delta;
pub use entry::{Direction, Entry};
pub use error::LedgerError;
pub use transaction::{BalancedTransaction, MAX_ENTRY_AMOUNT, MAX_TRANSACTION_ENTRIES};
