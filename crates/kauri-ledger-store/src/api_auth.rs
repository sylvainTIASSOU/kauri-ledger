use sha2::{Digest, Sha256};
use sqlx::PgPool;
use thiserror::Error;
use uuid::Uuid;

/// Validated API key representation without the raw secret.
pub struct ApiKeyProof {
    prefix: String,
    key_hash: [u8; 32],
}

impl ApiKeyProof {
    /// Parse the exact "Authorization: Bearer <token>" format.
    ///
    /// Canonical v1 tokens use a 43-character base62 payload.
    pub fn from_authorization(value: &str) -> Result<Self, ApiAuthError> {
        let token = value
            .strip_prefix("Bearer ")
            .ok_or(ApiAuthError::InvalidAuthorization)?;

        let payload = token
            .strip_prefix("lk_live_")
            .or_else(|| token.strip_prefix("lk_test_"))
            .ok_or(ApiAuthError::InvalidAuthorization)?;

        if payload.len() != 43 || !payload.bytes().all(|byte| byte.is_ascii_alphanumeric()) {
            return Err(ApiAuthError::InvalidAuthorization);
        }

        let key_hash: [u8; 32] = Sha256::digest(token.as_bytes()).into();

        Ok(Self {
            prefix: token[..16].to_owned(),
            key_hash,
        })
    }

    pub fn prefix(&self) -> &str {
        &self.prefix
    }

    pub fn hash_bytes(&self) -> &[u8] {
        &self.key_hash
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApiScope {
    Read,
    Write,
    Recon,
}

impl ApiScope {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
            Self::Recon => "recon",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApiAccess {
    Allowed,
    Unauthenticated,
    NotFound,
    Forbidden,
}

#[derive(Debug, Error)]
pub enum ApiAuthError {
    #[error("INVALID_AUTHORIZATION")]
    InvalidAuthorization,

    #[error("DATABASE_ERROR: {0}")]
    Database(#[from] sqlx::Error),

    #[error("INVALID_ACCESS_RESULT: {0}")]
    UnexpectedAccessResult(String),
}

/// Check a key's current permission without exposing key records.
///
/// The database performs a second authorization check inside
/// every privileged accounting wrapper.
pub async fn check_api_access(
    pool: &PgPool,
    proof: &ApiKeyProof,
    ledger_id: Uuid,
    scope: ApiScope,
) -> Result<ApiAccess, ApiAuthError> {
    let result: String = sqlx::query_scalar("SELECT ledger.api_key_access($1, $2, $3, $4)")
        .bind(proof.prefix())
        .bind(proof.hash_bytes())
        .bind(ledger_id)
        .bind(scope.as_str())
        .fetch_one(pool)
        .await?;

    match result.as_str() {
        "allowed" => Ok(ApiAccess::Allowed),
        "unauthenticated" => Ok(ApiAccess::Unauthenticated),
        "not_found" => Ok(ApiAccess::NotFound),
        "forbidden" => Ok(ApiAccess::Forbidden),
        _ => Err(ApiAuthError::UnexpectedAccessResult(result)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_token() -> String {
        format!("lk_test_{}", "A".repeat(43))
    }

    #[test]
    fn accepts_a_canonical_bearer_token() {
        let token = valid_token();
        let header = format!("Bearer {token}");

        let proof = ApiKeyProof::from_authorization(&header).unwrap();

        assert_eq!(proof.prefix(), &token[..16]);
        assert_eq!(proof.hash_bytes().len(), 32);

        let expected: [u8; 32] = Sha256::digest(token.as_bytes()).into();

        assert_eq!(proof.hash_bytes(), expected);
    }

    #[test]
    fn rejects_invalid_authorization_headers() {
        let token = valid_token();

        let invalid = [
            token.clone(),
            format!("Basic {token}"),
            format!("bearer {token}"),
            format!("Bearer  {token}"),
            "Bearer lk_test_short".to_owned(),
            format!("Bearer lk_test_{}", "!".repeat(43)),
            format!("Bearer lk_admin_{}", "A".repeat(43)),
        ];

        for value in invalid {
            assert!(
                ApiKeyProof::from_authorization(&value).is_err(),
                "Invalid authorization header was accepted"
            );
        }
    }
}
