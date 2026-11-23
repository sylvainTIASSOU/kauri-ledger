mod audit;

use clap::{Parser, Subcommand};
use kauri_ledger_core::AccountCode;
use kauri_ledger_store::{create_pool, verify_account_balance_integrity};
use uuid::Uuid;

#[derive(Parser)]
#[command(name = "kauri-cli")]
#[command(about = "Command-line interface for Kauri Ledger")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Verify the integrity of account balances
    VerifyIntegrity {
        /// PostgreSQL database URL
        #[arg(short, long, env = "DATABASE_URL")]
        database_url: String,

        /// Account code to verify (if not provided, verifies all accounts)
        #[arg(short, long)]
        account: Option<String>,
    },
    /// Audit the structural and accounting integrity of ledgers.
    AuditLedger {
        /// PostgreSQL database URL.
        #[arg(short, long, env = "DATABASE_URL")]
        database_url: String,

        /// Target one ledger, or audit all ledgers when omitted.
        #[arg(long)]
        ledger: Option<Uuid>,

        /// Output format for the audit report.
        #[arg(long, value_enum, default_value = "text")]
        format: audit::AuditFormat,
    },
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();

    match cli.command {
        Commands::VerifyIntegrity {
            database_url,
            account,
        } => {
            let pool = create_pool(&database_url).await?;

            if let Some(account_code_str) = account {
                let account_code = AccountCode::new(account_code_str)?;
                println!("Verifying integrity for account: {}", account_code.as_str());

                match verify_account_balance_integrity(&pool, &account_code).await {
                    Ok(()) => {
                        println!("✓ Account {} is consistent", account_code.as_str());
                        Ok(())
                    }
                    Err(e) => {
                        eprintln!("✗ Integrity error: {}", e);
                        std::process::exit(1);
                    }
                }
            } else {
                println!("Verifying integrity for all accounts...");

                // Fetch all accounts
                let accounts: Vec<(String,)> =
                    sqlx::query_as::<_, (String,)>("SELECT code FROM accounts")
                        .fetch_all(&pool)
                        .await?;

                let mut errors = Vec::new();

                for (code,) in accounts {
                    let account_code = AccountCode::new(&code)?;
                    if let Err(e) = verify_account_balance_integrity(&pool, &account_code).await {
                        errors.push((code, e.to_string()));
                    }
                }

                if errors.is_empty() {
                    println!("✓ All accounts are consistent");
                    Ok(())
                } else {
                    eprintln!("✗ Found {} integrity errors:", errors.len());
                    for (code, error) in errors {
                        eprintln!("  - {}: {}", code, error);
                    }
                    std::process::exit(1);
                }
            }
        }
        Commands::AuditLedger {
            database_url,
            ledger,
            format,
        } => {
            let pool = create_pool(&database_url).await?;

            let exit_code = audit::execute(&pool, ledger, format).await?;

            if exit_code != 0 {
                std::process::exit(exit_code);
            }

            Ok(())
        }
    }
}
