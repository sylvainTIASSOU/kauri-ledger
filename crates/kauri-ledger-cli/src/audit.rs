use clap::ValueEnum;
use kauri_ledger_store::integrity::{LedgerIntegrityReport, verify_ledger_integrity};
use sqlx::PgPool;
use std::error::Error;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum AuditFormat {
    Text,
    Json,
}

const EXIT_HEALTHY: i32 = 0;
const EXIT_INCONSISTENT: i32 = 2;
const EXIT_NOT_FOUND: i32 = 3;

/// Audit one ledger or all ledgers accessible to the database role.
///
/// Each ledger is checked using a consistent read-only snapshot.
/// An all-ledger audit uses a separate snapshot for each ledger.
pub async fn execute(
    pool: &PgPool,
    selected_ledger: Option<Uuid>,
    format: AuditFormat,
) -> Result<i32, Box<dyn Error>> {
    let ledger_ids: Vec<Uuid> = match selected_ledger {
        Some(ledger_id) => vec![ledger_id],
        None => {
            sqlx::query_scalar("SELECT id FROM ledger.ledgers ORDER BY id")
                .fetch_all(pool)
                .await?
        }
    };

    let mut reports: Vec<LedgerIntegrityReport> = Vec::with_capacity(ledger_ids.len());

    for ledger_id in ledger_ids {
        match verify_ledger_integrity(pool, ledger_id).await? {
            Some(report) => reports.push(report),
            None => {
                match format {
                    AuditFormat::Text => {
                        eprintln!("LEDGER_NOT_FOUND ledger_id={ledger_id}");
                    }
                    AuditFormat::Json => {
                        let error = serde_json::json!({
                            "error": "LEDGER_NOT_FOUND",
                            "ledger_id": ledger_id,
                        });

                        println!("{}", serde_json::to_string_pretty(&error)?);
                    }
                }

                return Ok(EXIT_NOT_FOUND);
            }
        }
    }

    let healthy = reports.iter().all(LedgerIntegrityReport::is_healthy);

    match format {
        AuditFormat::Text => {
            print_text_report(&reports, healthy);
        }
        AuditFormat::Json => {
            print_json_report(&reports, healthy)?;
        }
    }

    if healthy {
        Ok(EXIT_HEALTHY)
    } else {
        Ok(EXIT_INCONSISTENT)
    }
}

fn print_text_report(reports: &[LedgerIntegrityReport], healthy: bool) {
    let overall_status = if healthy { "HEALTHY" } else { "UNHEALTHY" };

    println!("AUDIT {overall_status} ledgers_checked={}", reports.len());

    for report in reports {
        let status = if report.is_healthy() {
            "HEALTHY"
        } else {
            "UNHEALTHY"
        };

        println!(
            "LEDGER {status} id={} accounts={} \
             mismatches={} structural_issues={} \
             stored_net={} journal_net={}",
            report.ledger_id,
            report.accounts_checked,
            report.mismatches.len(),
            report.structural_issues.len(),
            report.stored_net_total,
            report.journal_net_total,
        );

        for mismatch in &report.mismatches {
            println!(
                "  ACCOUNT_MISMATCH account_id={} code={} \
                 stored_debit={:?} stored_credit={:?} \
                 journal_debit={} journal_credit={}",
                mismatch.account_id,
                mismatch.account_code,
                mismatch.stored_debit_total,
                mismatch.stored_credit_total,
                mismatch.journal_debit_total,
                mismatch.journal_credit_total,
            );
        }

        for issue in &report.structural_issues {
            println!("  STRUCTURAL_ISSUE {issue:?}");
        }
    }
}

fn print_json_report(
    reports: &[LedgerIntegrityReport],
    healthy: bool,
) -> Result<(), serde_json::Error> {
    // Accounting totals are represented as decimal strings.
    let ledgers: Vec<_> = reports
        .iter()
        .map(|report| {
            serde_json::json!({
                "ledger_id": report.ledger_id,
                "healthy": report.is_healthy(),
                "accounts_checked": report.accounts_checked,
                "stored_net_total": report.stored_net_total.to_string(),
                "journal_net_total": report.journal_net_total.to_string(),
                "mismatches": &report.mismatches,
                "structural_issues": &report.structural_issues,
            })
        })
        .collect();

    let document = serde_json::json!({
        "healthy": healthy,
        "ledgers_checked": reports.len(),
        "ledgers": ledgers,
    });

    println!("{}", serde_json::to_string_pretty(&document)?);
    Ok(())
}
