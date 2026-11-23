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

/// A mismatch between an account's stored totals and its journal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountTotalsMismatch {
    pub account_id: Uuid,
    pub account_code: String,
    pub stored_debit_total: Option<i64>,
    pub stored_credit_total: Option<i64>,
    pub journal_debit_total: i128,
    pub journal_credit_total: i128,
}

/// Read-only integrity report for one ledger.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LedgerIntegrityReport {
    pub ledger_id: Uuid,
    pub accounts_checked: usize,
    pub stored_net_total: i128,
    pub journal_net_total: i128,
    pub mismatches: Vec<AccountTotalsMismatch>,
}

impl LedgerIntegrityReport {
    /// Both representations must balance and agree per account.
    pub fn is_healthy(&self) -> bool {
        self.stored_net_total == 0 && self.journal_net_total == 0 && self.mismatches.is_empty()
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

        accumulate_net(
            &mut journal_net_total,
            journal_debit_total,
            journal_credit_total,
        )?;

        accumulate_net(
            &mut stored_net_total,
            i128::from(stored_debit_total.unwrap_or(0)),
            i128::from(stored_credit_total.unwrap_or(0)),
        )?;

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

    Ok(Some(LedgerIntegrityReport {
        ledger_id,
        accounts_checked,
        stored_net_total,
        journal_net_total,
        mismatches,
    }))
}
