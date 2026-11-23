use serde::de::{self, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};
use serde_json::{Map, Number, Value};
use sha2::{Digest, Sha256};
use std::fmt;
use thiserror::Error;

/// Largest integer safely interoperable with ECMAScript numbers.
const MAX_SAFE_JSON_INTEGER: i64 = 9_007_199_254_740_991;

#[derive(Debug, Error)]
pub enum RequestHashError {
    #[error("INVALID_JSON: {0}")]
    InvalidJson(#[from] serde_json::Error),

    #[error("JCS_SERIALIZATION_FAILED: {0}")]
    Canonicalization(serde_json::Error),
}

/// JSON value that rejects duplicate keys at every nesting level.
struct StrictJson(Value);

impl<'de> Deserialize<'de> for StrictJson {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct StrictJsonVisitor;

        impl<'de> Visitor<'de> for StrictJsonVisitor {
            type Value = StrictJson;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a valid I-JSON value without duplicate keys")
            }

            fn visit_bool<E>(self, value: bool) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                Ok(StrictJson(Value::Bool(value)))
            }

            fn visit_unit<E>(self) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                Ok(StrictJson(Value::Null))
            }

            fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                if !(-MAX_SAFE_JSON_INTEGER..=MAX_SAFE_JSON_INTEGER).contains(&value) {
                    return Err(E::custom("UNSAFE_JSON_INTEGER"));
                }

                Ok(StrictJson(Value::Number(Number::from(value))))
            }

            fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                if value > MAX_SAFE_JSON_INTEGER as u64 {
                    return Err(E::custom("UNSAFE_JSON_INTEGER"));
                }

                Ok(StrictJson(Value::Number(Number::from(value))))
            }

            fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                let number =
                    Number::from_f64(value).ok_or_else(|| E::custom("INVALID_JSON_NUMBER"))?;

                Ok(StrictJson(Value::Number(number)))
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                Ok(StrictJson(Value::String(value.to_owned())))
            }

            fn visit_string<E>(self, value: String) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                Ok(StrictJson(Value::String(value)))
            }

            fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                let mut values = Vec::new();

                while let Some(value) = sequence.next_element::<StrictJson>()? {
                    values.push(value.0);
                }

                Ok(StrictJson(Value::Array(values)))
            }

            fn visit_map<A>(self, mut object: A) -> Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut values = Map::new();

                while let Some((key, value)) = object.next_entry::<String, StrictJson>()? {
                    if values.contains_key(&key) {
                        return Err(de::Error::custom("DUPLICATE_JSON_KEY"));
                    }

                    values.insert(key, value.0);
                }

                Ok(StrictJson(Value::Object(values)))
            }
        }

        deserializer.deserialize_any(StrictJsonVisitor)
    }
}

/// Compute the canonical idempotency hash defined by the ledger
/// specification:
///
/// SHA-256(method || path || JCS(raw_request_body))
///
/// The body must be captured before server-side defaults or other
/// transformations are applied.
///
/// The method and path must be taken from the authenticated HTTP
/// request, not from arbitrary client-provided JSON fields.
pub fn compute_request_hash(
    method: &str,
    path: &str,
    raw_body: &[u8],
) -> Result<[u8; 32], RequestHashError> {
    let mut deserializer = serde_json::Deserializer::from_slice(raw_body);

    let parsed = StrictJson::deserialize(&mut deserializer)?;

    // Reject extra JSON values or trailing non-whitespace data.
    deserializer.end()?;

    let canonical_body =
        serde_json_canonicalizer::to_vec(&parsed.0).map_err(RequestHashError::Canonicalization)?;

    let mut hasher = Sha256::new();
    hasher.update(method.as_bytes());
    hasher.update(path.as_bytes());
    hasher.update(&canonical_body);

    Ok(hasher.finalize().into())
}
