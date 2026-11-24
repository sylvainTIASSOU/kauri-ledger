use serde::{Serialize, Serializer};
use sqlx::PgPool;
use std::num::ParseIntError;
use thiserror::Error;
use uuid::Uuid;

/// Database row containing stored totals and aggregated journal totals.
type AccountIntegrityRow = (Uuid, String, Option<i64>, Option<i64>, String, String);

#[derive(Debug, Error)]
pub enum IntegrityCheckError {
    #[error("DATABASE_ERROR: {0}")]
    Database(#[from] sqlx::Error),

    #[error("INVALID_JOURNAL_TOTAL: {0}")]
    InvalidJournalTotal(#[from] ParseIntError),

    #[error("INTEGRITY_SUM_OVERFLOW")]
    SumOverflow,
}

/// Serialize large accounting totals without losing integer precision.
fn serialize_i128_as_string<S>(value: &i128, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    serializer.serialize_str(&value.to_string())
}

/// A mismatch between an account's stored totals and its journal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AccountTotalsMismatch {
    pub account_id: Uuid,
    pub account_code: String,
    pub stored_debit_total: Option<i64>,
    pub stored_credit_total: Option<i64>,
    #[serde(serialize_with = "serialize_i128_as_string")]
    pub journal_debit_total: i128,
    #[serde(serialize_with = "serialize_i128_as_string")]
    pub journal_credit_total: i128,
}

/// Structural inconsistencies detected in an individual ledger.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LedgerStructuralIssue {
    BalanceWithoutAccount {
        account_id: Uuid,
    },
    EntryWithoutAccount {
        entry_id: i64,
        account_id: Uuid,
    },
    EntryWithoutTransaction {
        entry_id: i64,
        transaction_id: Uuid,
    },
    MissingTransactionSeal {
        transaction_id: Uuid,
    },

    SealWithoutTransaction {
        transaction_id: Uuid,
    },
    InvalidTransaction {
        transaction_id: Uuid,
        entry_count: i64,

        #[serde(serialize_with = "serialize_i128_as_string")]
        debit_total: i128,

        #[serde(serialize_with = "serialize_i128_as_string")]
        credit_total: i128,
    },
}

/// Read-only integrity report for one ledger.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LedgerIntegrityReport {
    pub ledger_id: Uuid,
    pub accounts_checked: usize,
    #[serde(serialize_with = "serialize_i128_as_string")]
    pub stored_net_total: i128,
    #[serde(serialize_with = "serialize_i128_as_string")]
    pub journal_net_total: i128,
    pub mismatches: Vec<AccountTotalsMismatch>,
    pub structural_issues: Vec<LedgerStructuralIssue>,
}

impl LedgerIntegrityReport {
    /// Both representations must balance and agree per account.
    pub fn is_healthy(&self) -> bool {
        self.stored_net_total == 0
            && self.journal_net_total == 0
            && self.mismatches.is_empty()
            && self.structural_issues.is_empty()
    }
}

fn accumulate_net(total: &mut i128, debit: i128, credit: i128) -> Result<(), IntegrityCheckError> {
    *total = total
        .checked_add(debit)
        .and_then(|value| value.checked_sub(credit))
        .ok_or(IntegrityCheckError::SumOverflow)?;

    Ok(())
}

/// Reconcile stored account totals against posted journal entries.
///
/// Returns None when the ledger does not exist.
///
/// A repeatable-read, read-only transaction guarantees that both
/// queries observe one consistent database snapshot.
///
/// The database connection must already have authorization to read
/// the requested ledger. This function does not enforce tenant
/// authentication.
pub async fn verify_ledger_integrity(
    pool: &PgPool,
    ledger_id: Uuid,
) -> Result<Option<LedgerIntegrityReport>, IntegrityCheckError> {
    let mut transaction = pool.begin().await?;

    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
        .execute(&mut *transaction)
        .await?;

    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS( \
            SELECT 1 FROM ledger.ledgers WHERE id = $1 \
        )",
    )
    .bind(ledger_id)
    .fetch_one(&mut *transaction)
    .await?;

    // Detect balance rows without a corresponding account.
    let orphan_balances: Vec<Uuid> = sqlx::query_scalar(
        r#"
        SELECT b.account_id
        FROM ledger.account_balances AS b
        LEFT JOIN ledger.accounts AS a
          ON a.ledger_id = b.ledger_id
         AND a.id = b.account_id
        WHERE b.ledger_id = $1
          AND a.id IS NULL
        ORDER BY b.account_id
        "#,
    )
    .bind(ledger_id)
    .fetch_all(&mut *transaction)
    .await?;

    // Detect journal entries pointing to missing accounts.
    let orphan_entry_accounts: Vec<(i64, Uuid)> = sqlx::query_as(
        r#"
            SELECT e.id, e.account_id
            FROM ledger.entries AS e
            LEFT JOIN ledger.accounts AS a
              ON a.ledger_id = e.ledger_id
             AND a.id = e.account_id
            WHERE e.ledger_id = $1
              AND a.id IS NULL
            ORDER BY e.id
            "#,
    )
    .bind(ledger_id)
    .fetch_all(&mut *transaction)
    .await?;

    // Detect entries pointing to missing transactions.
    let orphan_entry_transactions: Vec<(i64, Uuid)> = sqlx::query_as(
        r#"
            SELECT e.id, e.transaction_id
            FROM ledger.entries AS e
            LEFT JOIN ledger.transactions AS t
              ON t.ledger_id = e.ledger_id
             AND t.id = e.transaction_id
            WHERE e.ledger_id = $1
              AND t.id IS NULL
            ORDER BY e.id
            "#,
    )
    .bind(ledger_id)
    .fetch_all(&mut *transaction)
    .await?;

    // Verify that every posted transaction has 2-64 entries
    // and that its debits equal its credits.
    let transaction_rows: Vec<(Uuid, i64, String, String)> = sqlx::query_as(
        r#"
            SELECT
                t.id,
                COUNT(e.id)::bigint,
                COALESCE(
                    SUM(e.amount::numeric)
                    FILTER (WHERE e.side = 'D'),
                    0
                )::text,
                COALESCE(
                    SUM(e.amount::numeric)
                    FILTER (WHERE e.side = 'C'),
                    0
                )::text
            FROM ledger.transactions AS t
            LEFT JOIN ledger.entries AS e
              ON e.ledger_id = t.ledger_id
             AND e.transaction_id = t.id
            WHERE t.ledger_id = $1
            GROUP BY t.id
            ORDER BY t.id
            "#,
    )
    .bind(ledger_id)
    .fetch_all(&mut *transaction)
    .await?;

    // Every committed transaction must have a validation seal.
    let missing_seals: Vec<Uuid> = sqlx::query_scalar(
        r#"
        SELECT t.id
        FROM ledger.transactions AS t
        LEFT JOIN ledger.transaction_seals AS s
          ON s.ledger_id = t.ledger_id
         AND s.transaction_id = t.id
        WHERE t.ledger_id = $1
          AND s.transaction_id IS NULL
        ORDER BY t.id
        "#,
    )
    .bind(ledger_id)
    .fetch_all(&mut *transaction)
    .await?;

    // Detect seals whose parent transaction no longer exists.
    let orphan_seals: Vec<Uuid> = sqlx::query_scalar(
        r#"
        SELECT s.transaction_id
        FROM ledger.transaction_seals AS s
        LEFT JOIN ledger.transactions AS t
          ON t.ledger_id = s.ledger_id
         AND t.id = s.transaction_id
        WHERE s.ledger_id = $1
          AND t.id IS NULL
        ORDER BY s.transaction_id
        "#,
    )
    .bind(ledger_id)
    .fetch_all(&mut *transaction)
    .await?;

    // Include orphan data when calculating global conservation.
    let stored_totals: (String, String) = sqlx::query_as(
        r#"
        SELECT
            COALESCE(SUM(debit_total::numeric), 0)::text,
            COALESCE(SUM(credit_total::numeric), 0)::text
        FROM ledger.account_balances
        WHERE ledger_id = $1
        "#,
    )
    .bind(ledger_id)
    .fetch_one(&mut *transaction)
    .await?;

    let journal_totals: (String, String) = sqlx::query_as(
        r#"
        SELECT
            COALESCE(
                SUM(amount::numeric)
                FILTER (WHERE side = 'D'),
                0
            )::text,
            COALESCE(
                SUM(amount::numeric)
                FILTER (WHERE side = 'C'),
                0
            )::text
        FROM ledger.entries
        WHERE ledger_id = $1
        "#,
    )
    .bind(ledger_id)
    .fetch_one(&mut *transaction)
    .await?;

    if !exists {
        transaction.commit().await?;
        return Ok(None);
    }

    // Aggregate journal movements before joining account balances.
    // Numeric arithmetic avoids overflow while summing BIGINT entries.
    let rows: Vec<AccountIntegrityRow> = sqlx::query_as(
        r#"
        WITH journal_totals AS (
            SELECT
                e.account_id,
                COALESCE(
                    SUM(e.amount::numeric)
                    FILTER (WHERE e.side = 'D'),
                    0
                )::text AS debit_total,
                COALESCE(
                    SUM(e.amount::numeric)
                    FILTER (WHERE e.side = 'C'),
                    0
                )::text AS credit_total
            FROM ledger.entries AS e
            WHERE e.ledger_id = $1
            GROUP BY e.account_id
        )
        SELECT
            a.id,
            a.code,
            b.debit_total,
            b.credit_total,
            COALESCE(j.debit_total, '0'),
            COALESCE(j.credit_total, '0')
        FROM ledger.accounts AS a
        LEFT JOIN ledger.account_balances AS b
            ON b.ledger_id = a.ledger_id
           AND b.account_id = a.id
        LEFT JOIN journal_totals AS j
            ON j.account_id = a.id
        WHERE a.ledger_id = $1
        ORDER BY a.id
        "#,
    )
    .bind(ledger_id)
    .fetch_all(&mut *transaction)
    .await?;

    transaction.commit().await?;

    let accounts_checked = rows.len();
    let mut mismatches = Vec::new();

    let mut stored_net_total = 0_i128;
    let mut journal_net_total = 0_i128;

    accumulate_net(
        &mut stored_net_total,
        stored_totals.0.parse()?,
        stored_totals.1.parse()?,
    )?;

    accumulate_net(
        &mut journal_net_total,
        journal_totals.0.parse()?,
        journal_totals.1.parse()?,
    )?;

    let mut structural_issues = Vec::new();

    for account_id in orphan_balances {
        structural_issues.push(LedgerStructuralIssue::BalanceWithoutAccount { account_id });
    }

    for (entry_id, account_id) in orphan_entry_accounts {
        structural_issues.push(LedgerStructuralIssue::EntryWithoutAccount {
            entry_id,
            account_id,
        });
    }

    for (entry_id, transaction_id) in orphan_entry_transactions {
        structural_issues.push(LedgerStructuralIssue::EntryWithoutTransaction {
            entry_id,
            transaction_id,
        });
    }

    for (transaction_id, entry_count, debit_text, credit_text) in transaction_rows {
        let debit_total: i128 = debit_text.parse()?;
        let credit_total: i128 = credit_text.parse()?;

        if !(2..=64).contains(&entry_count) || debit_total != credit_total {
            structural_issues.push(LedgerStructuralIssue::InvalidTransaction {
                transaction_id,
                entry_count,
                debit_total,
                credit_total,
            });
        }
    }

    for (
        account_id,
        account_code,
        stored_debit_total,
        stored_credit_total,
        journal_debit_text,
        journal_credit_text,
    ) in rows
    {
        let journal_debit_total: i128 = journal_debit_text.parse()?;

        let journal_credit_total: i128 = journal_credit_text.parse()?;

        let matches_journal = stored_debit_total
            .is_some_and(|value| i128::from(value) == journal_debit_total)
            && stored_credit_total.is_some_and(|value| i128::from(value) == journal_credit_total);

        if !matches_journal {
            mismatches.push(AccountTotalsMismatch {
                account_id,
                account_code,
                stored_debit_total,
                stored_credit_total,
                journal_debit_total,
                journal_credit_total,
            });
        }
    }

    for transaction_id in missing_seals {
        structural_issues.push(LedgerStructuralIssue::MissingTransactionSeal { transaction_id });
    }

    for transaction_id in orphan_seals {
        structural_issues.push(LedgerStructuralIssue::SealWithoutTransaction { transaction_id });
    }

    Ok(Some(LedgerIntegrityReport {
        ledger_id,
        accounts_checked,
        stored_net_total,
        journal_net_total,
        mismatches,
        structural_issues,
    }))
}
