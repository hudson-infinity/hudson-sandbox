//! Private immutable source objects for admitted file writes. Never customer authority.
use crate::{OperationId, file_downloads::ReadScope, files::Upload};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("invalid file source metadata")]
pub struct InvalidSource;

/// Independently resolved operation ownership, separate from a stored reference.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SourceOwner {
    pub scope: ReadScope,
    pub operation_id: OperationId,
}
impl SourceOwner {
    pub fn validate(&self) -> Result<(), InvalidSource> {
        self.scope.validate().map_err(|_| InvalidSource)
    }
}

/// Persist the complete plan and reserve its byte budget before any storage PUT.
/// A new attempt must never replace an uncertain attempt during recovery.
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SourcePlan {
    pub version: u32,
    pub owner: SourceOwner,
    pub upload: Upload,
    pub source_attempt: OperationId,
    pub created_unix_ms: i64,
    pub write_expires_unix_ms: i64,
    pub expires_unix_ms: i64,
    pub delete_after_unix_ms: i64,
}
impl std::fmt::Debug for SourcePlan {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SourcePlan")
            .field("operation_id", &self.upload.operation_id)
            .field("source_attempt", &self.source_attempt)
            .field("size", &self.upload.size)
            .finish_non_exhaustive()
    }
}
impl SourcePlan {
    pub fn validate(&self) -> Result<(), InvalidSource> {
        self.owner.validate()?;
        self.upload.validate().map_err(|_| InvalidSource)?;
        if self.owner.operation_id != self.upload.operation_id
            || self.version != 1
            || self.created_unix_ms <= 0
            || self.write_expires_unix_ms <= self.created_unix_ms
            || self.expires_unix_ms < self.write_expires_unix_ms
            || self.delete_after_unix_ms < self.expires_unix_ms
        {
            return Err(InvalidSource);
        }
        Ok(())
    }
    pub fn object_key(&self) -> Result<String, InvalidSource> {
        self.validate()?;
        Ok(format!(
            "file-source/v1/{}/{}/{}/{}/{}/{}/{}/{}",
            self.owner.scope.project_id,
            self.owner.scope.sandbox_id,
            self.upload.operation_id,
            self.owner.scope.allocation_id,
            self.owner.scope.generation,
            self.owner.scope.host_id,
            self.owner.scope.host_epoch,
            self.source_attempt
        ))
    }
    pub fn metadata_digest(&self) -> Result<String, InvalidSource> {
        self.validate()?;
        let mut hash = Sha256::new();
        hash.update(b"hudson-file-source-v1\0");
        hash.update(serde_json::to_vec(self).map_err(|_| InvalidSource)?);
        Ok(hex::encode(hash.finalize()))
    }
}
fn identity(value: &str) -> Result<(), InvalidSource> {
    if value.is_empty() || value.len() > 1024 || value.chars().any(char::is_control) {
        Err(InvalidSource)
    } else {
        Ok(())
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SourceRef {
    pub plan: SourcePlan,
    pub etag: String,
    pub object_version: Option<String>,
}
impl SourceRef {
    pub fn validate(&self) -> Result<(), InvalidSource> {
        self.plan.validate()?;
        for value in std::iter::once(&self.etag).chain(self.object_version.iter()) {
            identity(value)?;
        }
        Ok(())
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SourceRetirement {
    pub version: u32,
    pub plan_sha256: String,
    pub previous: Option<SourceRef>,
    pub marker_etag: String,
    pub marker_version: Option<String>,
}
impl SourceRetirement {
    pub fn validate(&self, plan: &SourcePlan) -> Result<(), InvalidSource> {
        if self.version != 1 || self.plan_sha256 != plan.metadata_digest()? {
            return Err(InvalidSource);
        }
        for value in std::iter::once(&self.marker_etag).chain(self.marker_version.iter()) {
            identity(value)?;
        }
        if let Some(previous) = &self.previous {
            previous.validate()?;
            let compatible = match (
                previous.object_version.as_deref(),
                self.marker_version.as_deref(),
            ) {
                (None, None) | (Some("null"), Some("null")) => true,
                (Some(old), Some(marker)) => old != marker,
                _ => false,
            };
            if &previous.plan != plan || !compatible {
                return Err(InvalidSource);
            }
        }
        Ok(())
    }
}
