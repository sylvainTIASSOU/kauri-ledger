use chrono::{DateTime, Utc};
use clap::{Subcommand, ValueEnum};
use rand::{Rng, distr::Alphanumeric};
use sha2::{Digest, Sha256};
use sqlx::{FromRow, PgPool};
use std::{
    error::Error,
    io::{Error as IoError, ErrorKind},
};
use uuid::Uuid;
use zeroize::Zeroizing;

type CliResult<T> = Result<T, Box<dyn Error>>;

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum KeyRole {
    Read,
    Write,
    Recon,
}

impl KeyRole {
    fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
            Self::Recon => "recon",
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum KeyEnvironment {
    Test,
    Live,
}

impl KeyEnvironment {
    fn prefix(self) -> &'static str {
        match self {
            Self::Test => "lk_test_",
            Self::Live => "lk_live_",
        }
    }
}

#[derive(Debug, Subcommand)]
pub enum KeysCommand {
    /// Issue a new API key for one ledger.
    Create {
        #[arg(long)]
        ledger: Uuid,

        #[arg(long, value_enum)]
        role: KeyRole,

        #[arg(long)]
        label: String,

        #[arg(long, value_enum, default_value = "test")]
        environment: KeyEnvironment,

        /// "never" or an RFC3339 timestamp.
        #[arg(long, default_value = "never")]
        expires: String,

        /// Authenticated operator identity supplied by the control plane.
        #[arg(long, env = "LEDGER_ACTOR")]
        actor: String,
    },

    /// List API key metadata without revealing any secret.
    List {
        #[arg(long)]
        ledger: Option<Uuid>,
    },

    /// Revoke an API key immediately.
    Revoke {
        #[arg(long)]
        id: Uuid,

        #[arg(long, env = "LEDGER_ACTOR")]
        actor: String,
    },
}

#[derive(Debug, FromRow)]
struct KeyListRow {
    id: Uuid,
    ledger_id: Uuid,
    prefix: String,
    last4: String,
    scopes: Vec<String>,
    label: String,
    created_at: DateTime<Utc>,
    expires_at: Option<DateTime<Utc>>,
    revoked_at: Option<DateTime<Utc>>,
    last_used_at: Option<DateTime<Utc>>,
}

fn invalid_input(message: &'static str) -> Box<dyn Error> {
    Box::new(IoError::new(ErrorKind::InvalidInput, message))
}

fn valid_label(value: &str) -> bool {
    (1..=200).contains(&value.chars().count()) && !value.trim().is_empty()
}

fn parse_expiration(value: &str) -> CliResult<Option<DateTime<Utc>>> {
    if value == "never" {
        return Ok(None);
    }

    let expires_at = DateTime::parse_from_rfc3339(value)?.with_timezone(&Utc);

    if expires_at <= Utc::now() {
        return Err(invalid_input("API key expiration must be in the future"));
    }

    Ok(Some(expires_at))
}

/// Generate a 43-character base62 secret using a cryptographic RNG.
///
/// 43 independently sampled base62 characters provide approximately
/// 256 bits of entropy. The caller must never log the result.
fn generate_token(environment: KeyEnvironment) -> Zeroizing<String> {
    let prefix = environment.prefix();
    let mut token = Zeroizing::new(String::with_capacity(prefix.len() + 43));

    token.push_str(prefix);

    let mut rng = rand::rng();

    for _ in 0..43 {
        token.push(rng.sample(Alphanumeric) as char);
    }

    token
}

/// Reject a runtime-only database login.
///
/// SQL permissions remain authoritative. This check produces
/// a clear CLI error before an administrative write is attempted.
async fn require_admin_connection(pool: &PgPool) -> CliResult<()> {
    let allowed: bool = sqlx::query_scalar(
        r#"
        SELECT
            has_table_privilege(
                current_user, 'ledger.api_keys', 'SELECT'
            )
            AND has_table_privilege(
                current_user, 'ledger.api_keys', 'INSERT'
            )
            AND has_table_privilege(
                current_user, 'ledger.api_keys', 'UPDATE'
            )
            AND has_table_privilege(
                current_user, 'ledger.api_key_ledgers', 'SELECT'
            )
            AND has_table_privilege(
                current_user, 'ledger.api_key_ledgers', 'INSERT'
            )
            AND has_table_privilege(
                current_user, 'ledger.audit_log', 'INSERT'
            )
        "#,
    )
    .fetch_one(pool)
    .await?;

    if !allowed {
        return Err(invalid_input(
            "Administrative database privileges are required",
        ));
    }

    Ok(())
}

async fn create_key(
    pool: &PgPool,
    ledger_id: Uuid,
    role: KeyRole,
    label: String,
    environment: KeyEnvironment,
    expires: String,
    actor: String,
) -> CliResult<()> {
    if !valid_label(&label) || !valid_label(&actor) {
        return Err(invalid_input(
            "Label and actor must contain 1 to 200 characters",
        ));
    }

    let expires_at = parse_expiration(&expires)?;

    // Generate the secret before opening a database transaction.
    let token = generate_token(environment);

    let hash: [u8; 32] = Sha256::digest(token.as_bytes()).into();

    let prefix = &token[..16];
    let last4 = &token[token.len() - 4..];

    let key_id = Uuid::now_v7();
    let scopes = vec![role.as_str().to_owned()];

    let mut transaction = pool.begin().await?;

    // Only a hashed key is stored in PostgreSQL.
    sqlx::query(
        r#"
        INSERT INTO ledger.api_keys (
            id,
            key_hash,
            prefix,
            last4,
            scopes,
            label,
            expires_at
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7)
        "#,
    )
    .bind(key_id)
    .bind(hash.as_slice())
    .bind(prefix)
    .bind(last4)
    .bind(scopes)
    .bind(&label)
    .bind(expires_at)
    .execute(&mut *transaction)
    .await?;

    // The database enforces one ledger per key in v1.
    sqlx::query(
        r#"
        INSERT INTO ledger.api_key_ledgers (
            api_key_id,
            ledger_id
        )
        VALUES ($1, $2)
        "#,
    )
    .bind(key_id)
    .bind(ledger_id)
    .execute(&mut *transaction)
    .await?;

    // Key creation and its audit event are committed together.
    sqlx::query(
        r#"
        INSERT INTO ledger.audit_log (
            actor,
            action,
            target_type,
            target_id,
            ledger_id
        )
        VALUES (
            $1,
            'api_key_created',
            'api_key',
            $2,
            $3
        )
        "#,
    )
    .bind(&actor)
    .bind(key_id)
    .bind(ledger_id)
    .execute(&mut *transaction)
    .await?;

    transaction.commit().await?;

    // Metadata goes to stderr; only the secret goes to stdout.
    eprintln!(
        "CREATED id={key_id} ledger={ledger_id} role={} \
         prefix={prefix} last4={last4}",
        role.as_str(),
    );

    // The raw secret is displayed exactly once after COMMIT.
    println!("{}", token.as_str());

    Ok(())
}

async fn list_keys(pool: &PgPool, ledger_id: Option<Uuid>) -> CliResult<()> {
    let rows: Vec<KeyListRow> = sqlx::query_as(
        r#"
        SELECT
            k.id,
            kl.ledger_id,
            k.prefix,
            k.last4,
            k.scopes,
            k.label,
            k.created_at,
            k.expires_at,
            k.revoked_at,
            k.last_used_at
        FROM ledger.api_keys AS k
        JOIN ledger.api_key_ledgers AS kl
          ON kl.api_key_id = k.id
        WHERE (
            $1::uuid IS NULL
            OR kl.ledger_id = $1
        )
        ORDER BY k.created_at DESC, k.id
        "#,
    )
    .bind(ledger_id)
    .fetch_all(pool)
    .await?;

    let result: Vec<_> = rows
        .iter()
        .map(|row| {
            serde_json::json!({
                "id": row.id,
                "ledger_id": row.ledger_id,
                "prefix": &row.prefix,
                "last4": &row.last4,
                "scopes": &row.scopes,
                "label": &row.label,
                "created_at": row.created_at,
                "expires_at": row.expires_at,
                "revoked_at": row.revoked_at,
                "last_used_at": row.last_used_at,
            })
        })
        .collect();

    println!("{}", serde_json::to_string_pretty(&result)?);

    Ok(())
}

async fn revoke_key(pool: &PgPool, key_id: Uuid, actor: String) -> CliResult<()> {
    if !valid_label(&actor) {
        return Err(invalid_input("Actor must contain 1 to 200 characters"));
    }

    let mut transaction = pool.begin().await?;

    // Only revoke active keys. The operation is idempotent.
    let ledger_id: Option<Uuid> = sqlx::query_scalar(
        r#"
        UPDATE ledger.api_keys AS k
        SET revoked_at = NOW()
        FROM ledger.api_key_ledgers AS kl
        WHERE k.id = $1
          AND k.revoked_at IS NULL
          AND kl.api_key_id = k.id
        RETURNING kl.ledger_id
        "#,
    )
    .bind(key_id)
    .fetch_optional(&mut *transaction)
    .await?;

    if let Some(ledger_id) = ledger_id {
        sqlx::query(
            r#"
            INSERT INTO ledger.audit_log (
                actor,
                action,
                target_type,
                target_id,
                ledger_id
            )
            VALUES (
                $1,
                'api_key_revoked',
                'api_key',
                $2,
                $3
            )
            "#,
        )
        .bind(&actor)
        .bind(key_id)
        .bind(ledger_id)
        .execute(&mut *transaction)
        .await?;

        transaction.commit().await?;
        println!("REVOKED id={key_id}");
        return Ok(());
    }

    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS( \
             SELECT 1 FROM ledger.api_keys WHERE id = $1 \
         )",
    )
    .bind(key_id)
    .fetch_one(&mut *transaction)
    .await?;

    transaction.commit().await?;

    if exists {
        println!("ALREADY_REVOKED id={key_id}");
        Ok(())
    } else {
        Err(invalid_input("API key not found"))
    }
}

pub async fn execute(pool: &PgPool, command: KeysCommand) -> CliResult<()> {
    require_admin_connection(pool).await?;

    match command {
        KeysCommand::Create {
            ledger,
            role,
            label,
            environment,
            expires,
            actor,
        } => create_key(pool, ledger, role, label, environment, expires, actor).await,

        KeysCommand::List { ledger } => list_keys(pool, ledger).await,

        KeysCommand::Revoke { id, actor } => revoke_key(pool, id, actor).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_tokens_have_the_required_format() {
        let first = generate_token(KeyEnvironment::Test);
        let second = generate_token(KeyEnvironment::Test);
        let live = generate_token(KeyEnvironment::Live);

        assert_eq!(first.len(), 51);
        assert_eq!(second.len(), 51);
        assert_eq!(live.len(), 51);

        assert!(first.starts_with("lk_test_"));
        assert!(live.starts_with("lk_live_"));

        assert!(first[8..].bytes().all(|byte| byte.is_ascii_alphanumeric()));

        assert_ne!(first.as_str(), second.as_str());
    }

    #[test]
    fn expiration_must_be_in_the_future() {
        assert!(parse_expiration("never").unwrap().is_none());
        assert!(parse_expiration("invalid-date").is_err());
        assert!(parse_expiration("2020-01-01T00:00:00Z").is_err());
    }
}
