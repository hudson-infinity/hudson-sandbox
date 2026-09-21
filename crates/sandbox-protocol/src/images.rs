//! Operator authorization for immutable rootfs images. This does not verify bytes.
use std::{collections::BTreeSet, sync::Arc};

/// A canonical SHA-256 digest. All runtime boundaries use the same spelling.
pub fn valid_image_digest(digest: &str) -> bool {
    digest.strip_prefix("sha256:").is_some_and(|hex| {
        hex.len() == 64
            && hex
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ImagePolicyError {
    #[error("image policy must contain between 1 and 256 entries")]
    Size,
    #[error("image policy requires canonical lowercase sha256 digests")]
    Digest,
    #[error("image policy contains a duplicate digest")]
    Duplicate,
}

/// Explicit, immutable configuration supplied by the operator, never the caller.
/// There is no permissive default. Replace API state to install a new policy.
#[derive(Debug, Clone)]
pub struct ImageAllowlist(Arc<BTreeSet<String>>);

impl ImageAllowlist {
    pub fn new(digests: impl IntoIterator<Item = String>) -> Result<Self, ImagePolicyError> {
        let mut entries = BTreeSet::new();
        for digest in digests {
            if entries.len() >= 256 {
                return Err(ImagePolicyError::Size);
            }
            if !valid_image_digest(&digest) {
                return Err(ImagePolicyError::Digest);
            }
            if !entries.insert(digest) {
                return Err(ImagePolicyError::Duplicate);
            }
        }
        if entries.is_empty() {
            return Err(ImagePolicyError::Size);
        }
        Ok(Self(Arc::new(entries)))
    }

    pub fn allows(&self, digest: &str) -> bool {
        self.0.contains(digest)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn requires_explicit_bounded_configuration() {
        assert!(matches!(
            ImageAllowlist::new([]),
            Err(ImagePolicyError::Size)
        ));
        let make = |n| (0..n).map(|i| format!("sha256:{i:064x}"));
        assert!(ImageAllowlist::new(make(256)).is_ok());
        assert!(matches!(
            ImageAllowlist::new(make(257)),
            Err(ImagePolicyError::Size)
        ));
    }

    #[test]
    fn rejects_ambiguous_or_malformed_configuration() {
        let valid = format!("sha256:{}", "a".repeat(64));
        for value in [
            "latest".into(),
            "sha256:abc".into(),
            valid.to_uppercase(),
            format!("sha256:{}", "A".repeat(64)),
            format!("{valid}\n"),
            format!("sha256:{}", "g".repeat(64)),
        ] {
            assert!(matches!(
                ImageAllowlist::new([value]),
                Err(ImagePolicyError::Digest)
            ));
        }
        assert!(matches!(
            ImageAllowlist::new([valid.clone(), valid]),
            Err(ImagePolicyError::Duplicate)
        ));
    }

    #[test]
    fn allows_only_configured_digests() {
        let a = format!("sha256:{}", "a".repeat(64));
        let policy = ImageAllowlist::new([a.clone()]).unwrap();
        assert!(policy.allows(&a));
        assert!(!policy.allows(&format!("sha256:{}", "b".repeat(64))));
    }
}
