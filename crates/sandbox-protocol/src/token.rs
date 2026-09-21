//! Project API tokens.
//!
//! A token is an opaque bearer credential carrying two parts: a nonsecret key
//! identifier used to find the right row, and a 256-bit secret that is never
//! stored. PostgreSQL holds only the SHA-256 of the secret, so a database dump
//! does not yield working credentials.
//!
//! Splitting the identifier from the secret is what lets verification be a
//! single indexed lookup followed by one constant-time comparison, rather than
//! a scan that compares against every token in the installation.
//!
//! Both halves are hex. Base64url would be shorter, but its alphabet contains
//! `_`, which is also the separator — a token whose secret happened to encode
//! with an underscore would fail to parse, intermittently and only for some
//! tokens. One encoding that cannot collide with the delimiter is worth the
//! extra characters.
//!
//! Rendering happens once, at issuance. A token cannot be recovered afterwards
//! — [`ProjectToken`] deliberately has no `Display`, and its `Debug` redacts
//! the secret, so it cannot reach a log line by accident.

use std::fmt;

use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

/// The human-visible prefix, so a leaked token is recognizable in a scan.
const TOKEN_PREFIX: &str = "hsb";
/// Bytes of key identifier. Nonsecret; only needs to avoid collisions.
const KEY_ID_BYTES: usize = 8;
/// Bytes of secret. 256 bits, per `docs/auth-design.md`.
const SECRET_BYTES: usize = 32;

/// Why a string could not be read as a project token.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum TokenParseError {
    /// Not three `_`-separated parts.
    #[error("expected `hsb_<key-id>_<secret>`")]
    Malformed,
    /// The leading `hsb` was missing.
    #[error("missing `hsb` prefix")]
    WrongPrefix,
    /// The key identifier was not the expected hex length.
    #[error("key identifier is not {} hex characters", KEY_ID_BYTES * 2)]
    BadKeyId,
    /// The secret was not the expected hex length.
    #[error("secret is not {} hex characters", SECRET_BYTES * 2)]
    BadSecret,
}

/// The nonsecret half of a token: which credential is being presented.
///
/// Safe to log, store in plaintext, and return in audit records.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TokenKeyId(String);

impl TokenKeyId {
    /// Wrap an identifier read back from storage.
    ///
    /// Not for caller input: a key identifier presented by a client arrives
    /// inside a token and is validated by [`ProjectToken::parse`].
    #[must_use]
    pub fn from_stored(value: String) -> Self {
        Self(value)
    }

    /// The identifier as stored and looked up.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for TokenKeyId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The SHA-256 of a token secret, as stored in PostgreSQL.
#[derive(Clone, PartialEq, Eq)]
pub struct TokenHash([u8; 32]);

impl TokenHash {
    /// Read a hash back from the database.
    #[must_use]
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// The raw hash, for a `bytea` column.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Compare in constant time.
    ///
    /// A byte-by-byte comparison leaks how much of a guess was correct, which
    /// over enough attempts recovers the hash. `==` is the wrong operator here
    /// and this method exists so it is never reached for.
    #[must_use]
    pub fn verify(&self, other: &Self) -> bool {
        self.0.ct_eq(&other.0).into()
    }
}

impl fmt::Debug for TokenHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Hashes are not secrets, but printing one invites comparing them by
        // eye, which is how constant-time comparison gets quietly bypassed.
        f.write_str("TokenHash(..)")
    }
}

/// A freshly generated or freshly parsed token, secret included.
///
/// Hold one only as long as it takes to verify or to render it once. It has no
/// `Display`, so the only way to produce the string a caller sees is
/// [`ProjectToken::render_once`], which consumes it.
#[derive(Clone)]
pub struct ProjectToken {
    key_id: TokenKeyId,
    secret: [u8; SECRET_BYTES],
}

impl ProjectToken {
    /// Mint a token from the operating system's randomness.
    ///
    /// # Errors
    ///
    /// Returns an error when the platform's entropy source is unavailable,
    /// which must fail the request rather than fall back to a weaker source.
    pub fn generate() -> Result<Self, getrandom::Error> {
        let mut key_id = [0u8; KEY_ID_BYTES];
        let mut secret = [0u8; SECRET_BYTES];
        getrandom::fill(&mut key_id)?;
        getrandom::fill(&mut secret)?;

        Ok(Self {
            key_id: TokenKeyId(hex::encode(key_id)),
            secret,
        })
    }

    /// Read a token presented by a caller.
    ///
    /// # Errors
    ///
    /// Returns [`TokenParseError`] for anything not matching the issued shape.
    /// A parse failure is a `401`, and the response says no more than that.
    pub fn parse(value: &str) -> Result<Self, TokenParseError> {
        let mut parts = value.split('_');
        let (Some(prefix), Some(key_id), Some(secret), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err(TokenParseError::Malformed);
        };

        if prefix != TOKEN_PREFIX {
            return Err(TokenParseError::WrongPrefix);
        }
        if key_id.len() != KEY_ID_BYTES * 2 || hex::decode(key_id).is_err() {
            return Err(TokenParseError::BadKeyId);
        }

        if secret.len() != SECRET_BYTES * 2 {
            return Err(TokenParseError::BadSecret);
        }
        let secret: [u8; SECRET_BYTES] = hex::decode(secret)
            .map_err(|_| TokenParseError::BadSecret)?
            .try_into()
            .map_err(|_| TokenParseError::BadSecret)?;

        Ok(Self {
            key_id: TokenKeyId(key_id.to_owned()),
            secret,
        })
    }

    /// Which credential this is. Safe to log.
    #[must_use]
    pub fn key_id(&self) -> &TokenKeyId {
        &self.key_id
    }

    /// The hash to store, or to compare against a stored one.
    #[must_use]
    pub fn hash(&self) -> TokenHash {
        let mut hasher = Sha256::new();
        hasher.update(self.secret);
        TokenHash(hasher.finalize().into())
    }

    /// Render the string a caller keeps, consuming the token.
    ///
    /// Taking `self` by value is the point: the value cannot be rendered
    /// twice, which matches the contract that a token is shown once at
    /// issuance and never recoverable afterwards.
    #[must_use]
    pub fn render_once(self) -> String {
        format!(
            "{TOKEN_PREFIX}_{}_{}",
            self.key_id,
            hex::encode(self.secret)
        )
    }
}

impl fmt::Debug for ProjectToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The key id is safe to print and is what makes a log line useful.
        // The secret must never appear, including through a derived Debug on
        // some struct that happens to hold one.
        f.debug_struct("ProjectToken")
            .field("key_id", &self.key_id)
            .field("secret", &"<redacted>")
            .finish()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;

    #[test]
    fn round_trips_through_its_rendered_form() {
        let token = ProjectToken::generate().expect("generate");
        let key_id = token.key_id().clone();
        let hash = token.hash();

        let parsed = ProjectToken::parse(&token.render_once()).expect("parse");
        assert_eq!(parsed.key_id(), &key_id);
        assert!(parsed.hash().verify(&hash));
    }

    #[test]
    fn two_tokens_differ() {
        let a = ProjectToken::generate().expect("a");
        let b = ProjectToken::generate().expect("b");
        assert_ne!(a.key_id(), b.key_id());
        assert!(!a.hash().verify(&b.hash()));
    }

    #[test]
    fn a_different_secret_does_not_verify() {
        // Hex case changes do not change the secret bytes. Exercise every
        // possible first nibble, using a different value rather than a new case.
        for first in "0123456789abcdef".chars() {
            let original = format!("hsb_0000000000000000_{first}{}", "0".repeat(63));
            let replacement = if first == '0' { '1' } else { '0' };
            let altered = format!("hsb_0000000000000000_{replacement}{}", "0".repeat(63));
            let real = ProjectToken::parse(&original).expect("original parses");
            let forged = ProjectToken::parse(&altered).expect("altered parses");
            assert_eq!(forged.key_id(), real.key_id());
            assert!(
                !forged.hash().verify(&real.hash()),
                "a changed secret byte verified against the real hash"
            );
        }
    }

    #[test]
    fn rejects_malformed_values() {
        for (value, expected) in [
            ("", TokenParseError::Malformed),
            ("hsb", TokenParseError::Malformed),
            ("hsb_0011223344556677", TokenParseError::Malformed),
            ("hsb_a_b_c", TokenParseError::Malformed),
            // Guards the encoding choice: a secret containing the separator
            // would be indistinguishable from an extra field.
            ("hsb_0011223344556677_aa_bb", TokenParseError::Malformed),
            ("xxx_0011223344556677_AAAA", TokenParseError::WrongPrefix),
            ("hsb_zz_AAAA", TokenParseError::BadKeyId),
            ("hsb_gggggggggggggggg_AAAA", TokenParseError::BadKeyId),
            ("hsb_0011223344556677_short", TokenParseError::BadSecret),
            ("hsb_0011223344556677_!!!!", TokenParseError::BadSecret),
            (
                // Right length, not hex.
                "hsb_0011223344556677_zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz",
                TokenParseError::BadSecret,
            ),
        ] {
            assert_eq!(
                ProjectToken::parse(value).err(),
                Some(expected),
                "unexpected result for {value:?}"
            );
        }
    }

    #[test]
    fn debug_never_reveals_the_secret() {
        let token = ProjectToken::generate().expect("generate");
        let rendered = token.clone().render_once();
        let secret = rendered.rsplit_once('_').expect("has a secret").1;

        let debug = format!("{token:?}");
        assert!(
            debug.contains(token.key_id().as_str()),
            "key id should show"
        );
        assert!(!debug.contains(secret), "secret leaked through Debug");
    }

    #[test]
    fn hash_debug_reveals_nothing() {
        let token = ProjectToken::generate().expect("generate");
        let hash = token.hash();
        let debug = format!("{hash:?}");
        assert_eq!(debug, "TokenHash(..)");
        assert!(!debug.contains(&hex::encode(hash.as_bytes())));
    }
}
