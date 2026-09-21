//! Versioned guest file-transfer descriptors. Guest reports are not host security evidence.
use crate::{OperationId, guest_model::Context};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const MAX_FILE_BYTES: u64 = 8 * 1024 * 1024;
pub const MAX_CHUNK_BYTES: usize = 32 * 1024;
pub const MAX_TRANSFERS: usize = 128;
pub const MAX_RESERVED_BYTES: u64 = 64 * 1024 * 1024;
pub const STATE_DIRECTORY: &str = ".hudson-transfers";

/// Canonical UTF-8 path relative to the configured workspace; no shell expansion.
pub fn validate_path(path: &str) -> Result<()> {
    ensure!(
        !path.is_empty() && path.len() <= 4096,
        "invalid file path length"
    );
    ensure!(
        !path.bytes().any(|b| b < 32 || b == 127 || b == b'\\')
            && path
                .split('/')
                .all(|p| !p.is_empty() && p != "." && p != ".." && p.len() <= 255)
            && path.split('/').next() != Some(STATE_DIRECTORY),
        "invalid workspace-relative file path"
    );
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Upload {
    pub operation_id: OperationId,
    pub path: String,
    pub size: u64,
    pub sha256: [u8; 32],
    /// Only ordinary 0644 or executable 0755 files are supported.
    pub mode: u32,
}
impl Upload {
    pub fn validate(&self) -> Result<()> {
        validate_path(&self.path)?;
        ensure!(self.size <= MAX_FILE_BYTES, "file exceeds transfer limit");
        ensure!(matches!(self.mode, 0o644 | 0o755), "invalid file mode");
        Ok(())
    }
    pub fn digest(&self) -> Result<[u8; 32]> {
        self.validate()?;
        let mut hash = Sha256::new();
        hash.update(b"hudson-file-upload-v1\0");
        hash.update(serde_json::to_vec(self)?);
        Ok(hash.finalize().into())
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Staging,
    CommitIntent,
    Committed,
    Unknown,
    Aborted,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Receipt {
    pub version: u32,
    pub context: Context,
    pub upload: Upload,
    pub digest: [u8; 32],
    pub state: State,
}
impl Receipt {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.version == 1
                && self.context.generation > 0
                && !self.context.boot_id.is_empty()
                && self.context.boot_id.len() <= 64,
            "invalid file receipt context"
        );
        ensure!(
            self.digest == self.upload.digest()?,
            "invalid file receipt digest"
        );
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::Id;
    #[test]
    fn paths_are_canonical_and_cannot_name_internal_state() {
        for path in [
            "",
            "/tmp/x",
            "../x",
            "a/../b",
            "a/./b",
            "a//b",
            "a/",
            ".",
            "a\\b",
            "a\0b",
            "a\nb",
            ".hudson-transfers/x",
        ] {
            assert!(validate_path(path).is_err(), "accepted {path:?}");
        }
        for path in [
            "file",
            "dir/file.txt",
            "résultats/結果",
            "file with spaces",
            "a/.hidden",
        ] {
            validate_path(path).unwrap();
        }
        assert!(validate_path(&"a".repeat(256)).is_err());
    }
    #[test]
    fn descriptor_digest_binds_path_size_mode_content_and_operation() {
        let original = Upload {
            operation_id: OperationId::generate(),
            path: "result".into(),
            size: 1,
            sha256: [4; 32],
            mode: 0o644,
        };
        let digest = original.digest().unwrap();
        for change in 0..5 {
            let mut next = original.clone();
            match change {
                0 => next.operation_id = OperationId::generate(),
                1 => next.path = "another".into(),
                2 => next.size = 2,
                3 => next.sha256 = [5; 32],
                _ => next.mode = 0o755,
            }
            assert_ne!(digest, next.digest().unwrap());
        }
        let mut bad = original;
        bad.mode = 0o4755;
        assert!(bad.validate().is_err());
        bad.mode = 0o644;
        bad.size = MAX_FILE_BYTES + 1;
        assert!(bad.validate().is_err());
    }
}
