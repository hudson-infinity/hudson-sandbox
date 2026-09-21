//! Idempotency keys and request digests.
//!
//! The client supplies a key to identify one logical mutation before the first
//! response exists. The server stores a digest of the normalized request
//! alongside it, so a retry carrying different content conflicts instead of
//! quietly running something else under a key the caller believes is settled.
//!
//! The digest is versioned. Normalization will change — a new optional field,
//! a different default — and without a version an upgrade would silently
//! reinterpret keys issued before it.

use std::fmt;

use serde::Serialize;
use sha2::{Digest, Sha256};

/// The normalization this build produces. Bump when the rules below change.
pub const DIGEST_VERSION: i32 = 1;

/// Shortest key accepted, per `docs/api-contract.md`.
const MIN_KEY_LEN: usize = 16;
/// Longest key accepted.
const MAX_KEY_LEN: usize = 128;

/// Why a key was not accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum KeyError {
    /// Shorter than the documented minimum.
    #[error("an idempotency key must be at least {MIN_KEY_LEN} characters")]
    TooShort,
    /// Longer than the documented maximum.
    #[error("an idempotency key must be at most {MAX_KEY_LEN} characters")]
    TooLong,
    /// Contains something outside the accepted alphabet.
    #[error("an idempotency key may contain only letters, digits, `.`, `_` and `-`")]
    BadCharacter,
}

/// A caller's key for one logical mutation.
///
/// Opaque and case-sensitive. The alphabet is narrow so a key cannot carry
/// user text, and therefore cannot leak anything through a log line or an
/// audit record.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct IdempotencyKey(String);

impl IdempotencyKey {
    /// Validate a key presented by a caller.
    ///
    /// # Errors
    ///
    /// Returns [`KeyError`] when the key is the wrong length or carries a
    /// character outside the accepted set. Both are a `400`: the caller can
    /// fix them, and retrying unchanged cannot succeed.
    pub fn parse(value: &str) -> Result<Self, KeyError> {
        if value.len() < MIN_KEY_LEN {
            return Err(KeyError::TooShort);
        }
        if value.len() > MAX_KEY_LEN {
            return Err(KeyError::TooLong);
        }
        if !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
        {
            return Err(KeyError::BadCharacter);
        }

        Ok(Self(value.to_owned()))
    }

    /// The key as stored.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for IdempotencyKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A digest of the normalized request a key was first used for.
#[derive(Clone, PartialEq, Eq)]
pub struct RequestDigest([u8; 32]);

impl RequestDigest {
    /// Compute the digest for a request.
    ///
    /// Covers the method, the canonical resource target, and the validated
    /// payload. Object keys are sorted so field order in the caller's JSON is
    /// not significant; array order and string contents are preserved, because
    /// reordering a command's arguments changes what runs.
    ///
    /// # Errors
    ///
    /// Returns an error only if the payload cannot be serialized, which for a
    /// validated request means a bug rather than bad input.
    pub fn compute<T: Serialize>(
        method: &str,
        target: &str,
        payload: &T,
    ) -> Result<Self, serde_json::Error> {
        let value = serde_json::to_value(payload)?;
        let mut canonical = Vec::new();
        write_canonical(&value, &mut canonical);

        let mut hasher = Sha256::new();
        // Length-prefixed so that ("POST", "/a/b") and ("POST/a", "/b") cannot
        // produce the same digest.
        for part in [method.as_bytes(), target.as_bytes(), &canonical] {
            hasher.update((part.len() as u64).to_be_bytes());
            hasher.update(part);
        }

        Ok(Self(hasher.finalize().into()))
    }

    /// Read a digest back from the database.
    #[must_use]
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// The raw digest, for a `bytea` column.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Debug for RequestDigest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Short prefix: enough to correlate two log lines, not enough to
        // reconstruct a payload by comparing digests against guesses.
        write!(f, "RequestDigest({})", hex::encode(&self.0[..4]))
    }
}

/// Serialize a JSON value with object keys in sorted order.
///
/// `serde_json`'s own output preserves insertion order, which would make the
/// digest depend on how the caller happened to write their JSON.
fn write_canonical(value: &serde_json::Value, out: &mut Vec<u8>) {
    use serde_json::Value;

    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort_unstable();
            out.push(b'{');
            for (index, key) in keys.iter().enumerate() {
                if index > 0 {
                    out.push(b',');
                }
                out.extend_from_slice(serde_json::to_string(key).unwrap_or_default().as_bytes());
                out.push(b':');
                if let Some(child) = map.get(*key) {
                    write_canonical(child, out);
                }
            }
            out.push(b'}');
        }
        Value::Array(items) => {
            out.push(b'[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(b',');
                }
                write_canonical(item, out);
            }
            out.push(b']');
        }
        other => {
            out.extend_from_slice(serde_json::to_string(other).unwrap_or_default().as_bytes());
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;
    use serde_json::json;

    fn digest(payload: &serde_json::Value) -> RequestDigest {
        RequestDigest::compute("POST", "/v1/sandboxes", payload).expect("digest")
    }

    #[test]
    fn accepts_a_uuid_shaped_key() {
        let key = "80d6bfaa-7245-493b-8d08-2cdb2de9885c";
        assert_eq!(
            IdempotencyKey::parse(key).expect("valid").as_str(),
            key,
            "the documented example key must be accepted"
        );
    }

    #[test]
    fn rejects_keys_outside_the_contract() {
        assert_eq!(
            IdempotencyKey::parse("short").err(),
            Some(KeyError::TooShort)
        );
        assert_eq!(
            IdempotencyKey::parse(&"a".repeat(129)).err(),
            Some(KeyError::TooLong)
        );
        for bad in [
            "has spaces in it!!",
            "has/a/slash/xxxx",
            "emoji-🔑-key-here",
        ] {
            assert_eq!(
                IdempotencyKey::parse(bad).err(),
                Some(KeyError::BadCharacter),
                "accepted {bad:?}"
            );
        }
    }

    #[test]
    fn keys_are_case_sensitive() {
        let lower = IdempotencyKey::parse("abcdefghijklmnop").expect("valid");
        let upper = IdempotencyKey::parse("ABCDEFGHIJKLMNOP").expect("valid");
        assert_ne!(lower, upper);
    }

    #[test]
    fn field_order_does_not_change_the_digest() {
        let a = json!({"name": "one", "resources": {"vcpu": 2, "memory_mib": 1024}});
        let b = json!({"resources": {"memory_mib": 1024, "vcpu": 2}, "name": "one"});
        assert_eq!(digest(&a), digest(&b));
    }

    #[test]
    fn argument_order_does_change_the_digest() {
        // Reordering a command's arguments changes what runs, so it must not
        // be treated as the same request.
        let a = json!({"args": ["-rf", "/tmp/x"]});
        let b = json!({"args": ["/tmp/x", "-rf"]});
        assert_ne!(digest(&a), digest(&b));
    }

    #[test]
    fn a_changed_value_changes_the_digest() {
        assert_ne!(digest(&json!({"vcpu": 2})), digest(&json!({"vcpu": 4})));
    }

    #[test]
    fn an_added_field_changes_the_digest() {
        assert_ne!(
            digest(&json!({"vcpu": 2})),
            digest(&json!({"vcpu": 2, "name": "x"}))
        );
    }

    #[test]
    fn the_target_is_part_of_the_digest() {
        let payload = json!({"vcpu": 2});
        let a = RequestDigest::compute("POST", "/v1/sandboxes/a/execute", &payload).expect("a");
        let b = RequestDigest::compute("POST", "/v1/sandboxes/b/execute", &payload).expect("b");
        assert_ne!(
            a, b,
            "the same payload against two sandboxes is two requests"
        );
    }

    #[test]
    fn the_method_is_part_of_the_digest() {
        let payload = json!({});
        let a = RequestDigest::compute("POST", "/v1/x", &payload).expect("a");
        let b = RequestDigest::compute("DELETE", "/v1/x", &payload).expect("b");
        assert_ne!(a, b);
    }

    #[test]
    fn method_and_target_cannot_be_confused_for_each_other() {
        // Without length prefixes these would hash identically.
        let payload = json!({});
        let a = RequestDigest::compute("POST", "/v1/x", &payload).expect("a");
        let b = RequestDigest::compute("POS", "T/v1/x", &payload).expect("b");
        assert_ne!(a, b);
    }

    #[test]
    fn debug_shows_only_a_short_prefix() {
        let value = digest(&json!({"vcpu": 2}));
        let rendered = format!("{value:?}");
        assert_eq!(rendered.len(), "RequestDigest(".len() + 8 + 1);
        assert!(!rendered.contains(&hex::encode(value.as_bytes())));
    }
}
