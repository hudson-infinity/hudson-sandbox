//! Private output descriptors. These are storage metadata, never authorization.
use crate::{AllocationId, HostId, OperationId, ProjectId, SandboxId, command::MAX_OUTPUT};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const MAX_CHUNK: usize = 32 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputName {
    Stdout,
    Stderr,
}
impl OutputName {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Stdout => "stdout",
            Self::Stderr => "stderr",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutputOwner {
    pub project_id: ProjectId,
    pub sandbox_id: SandboxId,
    pub operation_id: OperationId,
    pub allocation_id: AllocationId,
    pub generation: i64,
    pub host_id: HostId,
    pub host_epoch: i64,
    pub boot_id: String,
}
impl OutputOwner {
    fn validate(&self) -> Result<(), InvalidOutput> {
        if self.generation <= 0
            || self.host_epoch <= 0
            || self.boot_id.is_empty()
            || self.boot_id.len() > 64
            || self.boot_id.chars().any(char::is_control)
        {
            return Err(InvalidOutput);
        }
        Ok(())
    }
}

/// Service-assigned identity/retention before collecting the final bytes.
/// Replacement workers must retain this ticket, including its attempt ID.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutputTicket {
    pub version: u32,
    pub owner: OutputOwner,
    pub upload_attempt: OperationId,
    pub output_limit: u64,
    pub created_unix_ms: i64,
    pub expires_unix_ms: i64,
    pub delete_after_unix_ms: i64,
}
impl OutputTicket {
    pub fn validate(&self) -> Result<(), InvalidOutput> {
        self.owner.validate()?;
        if self.version != 1
            || !(1..=MAX_OUTPUT).contains(&self.output_limit)
            || self.created_unix_ms <= 0
            || self.expires_unix_ms <= self.created_unix_ms
            || self.delete_after_unix_ms < self.expires_unix_ms
        {
            return Err(InvalidOutput);
        }
        Ok(())
    }

    pub fn validate_plans(&self, plans: &OutputPlans) -> Result<(), InvalidOutput> {
        self.validate()?;
        plans.validate(&self.owner, self.output_limit)?;
        let a = &plans.stdout;
        if a.upload_attempt != self.upload_attempt
            || a.created_unix_ms != self.created_unix_ms
            || a.expires_unix_ms != self.expires_unix_ms
            || a.delete_after_unix_ms != self.delete_after_unix_ms
        {
            return Err(InvalidOutput);
        }
        Ok(())
    }
}

/// Persist this plan before uploading. Reconciliation must reuse it exactly.
/// Only final captured output can use this format; live output uses guest reads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutputPlan {
    pub version: u32,
    pub owner: OutputOwner,
    pub upload_attempt: OperationId,
    pub name: OutputName,
    pub size: u64,
    pub sha256: String,
    pub seen: u64,
    pub truncated: bool,
    pub created_unix_ms: i64,
    pub expires_unix_ms: i64,
    /// Earliest eligible deletion, not evidence that deletion happened.
    pub delete_after_unix_ms: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("invalid output metadata")]
pub struct InvalidOutput;

impl OutputPlan {
    pub fn validate(&self) -> Result<(), InvalidOutput> {
        self.owner.validate()?;
        if self.version != 1
            || self.size > MAX_OUTPUT
            || self.seen < self.size
            || self.truncated != (self.seen > self.size)
            || self.sha256.len() != 64
            || !self
                .sha256
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            || self.created_unix_ms <= 0
            || self.expires_unix_ms <= self.created_unix_ms
            || self.delete_after_unix_ms < self.expires_unix_ms
        {
            return Err(InvalidOutput);
        }
        Ok(())
    }

    /// Customer strings never become path segments. A new attempt has a new
    /// key; publication must choose one attempt transactionally in the store.
    pub fn object_key(&self) -> Result<String, InvalidOutput> {
        self.validate()?;
        Ok(format!(
            "output/v1/{}/{}/{}/{}/{}/{}/{}/{}/{}",
            self.owner.project_id,
            self.owner.sandbox_id,
            self.owner.operation_id,
            self.owner.allocation_id,
            self.owner.generation,
            self.owner.host_id,
            self.owner.host_epoch,
            self.upload_attempt,
            self.name.as_str()
        ))
    }

    /// Bind all metadata, including boot and retention, to the stored object.
    pub fn metadata_digest(&self) -> Result<String, InvalidOutput> {
        self.validate()?;
        let encoded = serde_json::to_vec(self).map_err(|_| InvalidOutput)?;
        Ok(hex::encode(Sha256::digest(encoded)))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutputRef {
    pub plan: OutputPlan,
    pub etag: String,
    pub object_version: Option<String>,
}
impl OutputRef {
    pub fn validate(&self) -> Result<(), InvalidOutput> {
        self.plan.validate()?;
        for value in std::iter::once(&self.etag).chain(self.object_version.iter()) {
            if value.is_empty() || value.len() > 1024 || value.chars().any(char::is_control) {
                return Err(InvalidOutput);
            }
        }
        Ok(())
    }
}

/// Exactly two final streams, including explicit empty ones. Absence of a
/// reference is not proof of an empty successful stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutputRefs {
    pub stdout: OutputRef,
    pub stderr: OutputRef,
}

/// Persisted together before any external upload can be authorized.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutputPlans {
    pub stdout: OutputPlan,
    pub stderr: OutputPlan,
}
impl OutputPlans {
    pub fn validate(&self, owner: &OutputOwner, output_limit: u64) -> Result<(), InvalidOutput> {
        self.stdout.validate()?;
        self.stderr.validate()?;
        validate_pair(&self.stdout, &self.stderr, owner, output_limit)
    }
}

impl OutputRefs {
    pub fn validate(&self, owner: &OutputOwner, output_limit: u64) -> Result<(), InvalidOutput> {
        self.stdout.validate()?;
        self.stderr.validate()?;
        validate_pair(&self.stdout.plan, &self.stderr.plan, owner, output_limit)
    }

    pub fn plans(&self) -> OutputPlans {
        OutputPlans {
            stdout: self.stdout.plan.clone(),
            stderr: self.stderr.plan.clone(),
        }
    }
}

fn validate_pair(
    a: &OutputPlan,
    b: &OutputPlan,
    owner: &OutputOwner,
    output_limit: u64,
) -> Result<(), InvalidOutput> {
    if a.name != OutputName::Stdout
        || b.name != OutputName::Stderr
        || &a.owner != owner
        || &b.owner != owner
        || a.upload_attempt != b.upload_attempt
        || a.created_unix_ms != b.created_unix_ms
        || a.expires_unix_ms != b.expires_unix_ms
        || a.delete_after_unix_ms != b.delete_after_unix_ms
        || !(1..=MAX_OUTPUT).contains(&output_limit)
        || a.size + b.size > output_limit
    {
        return Err(InvalidOutput);
    }
    Ok(())
}
