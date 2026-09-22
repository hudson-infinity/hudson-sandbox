//! Frozen whole-allocation retirement scope. Shape validation is not authority,
//! consumer closure, physical release, or permission to delete metadata.
use crate::{Id, OperationId, allocation_authority::Permit};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const MAX_BYTES: usize = 8192;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    #[error("invalid allocation retirement request")]
    Invalid,
    #[error("allocation retirement scope changed")]
    Scope,
    #[error("allocation retirement claim expired or stale")]
    Claim,
}

/// Database-proposed closure, requiring independent host verification. Empty
/// means explicitly frozen empty, never an inference from a missing receipt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum DomainClosure {
    Empty {},
    Retired { through: OperationId },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Intent {
    pub version: u32,
    pub retirement: OperationId,
    pub permit: Permit,
    pub commands: DomainClosure,
    pub files: DomainClosure,
    /// Digest of the retained database release evidence; not a release proof.
    pub release_evidence_sha256: String,
    /// Simulation cannot be upgraded to physical evidence by a later retry.
    pub simulated: bool,
}
fn valid_id(id: OperationId) -> bool {
    id.uuid().get_version_num() == 7 && id.uuid().get_variant() == uuid::Variant::RFC4122
}
impl Intent {
    pub fn validate(&self) -> Result<(), Error> {
        if self.version != 1
            || !valid_id(self.retirement)
            || self.retirement == self.permit.create_operation
            || self.permit.validate().is_err()
            || self.release_evidence_sha256.len() != 64
            || !self
                .release_evidence_sha256
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(Error::Invalid);
        }
        for domain in [&self.commands, &self.files] {
            if let DomainClosure::Retired { through } = domain
                && (!valid_id(*through)
                    || *through == self.retirement
                    || *through == self.permit.create_operation)
            {
                return Err(Error::Invalid);
            }
        }
        Ok(())
    }
    pub fn encode(&self) -> Result<Vec<u8>, Error> {
        self.validate()?;
        let bytes = serde_json::to_vec(self).map_err(|_| Error::Invalid)?;
        if bytes.len() > MAX_BYTES {
            return Err(Error::Invalid);
        }
        Ok(bytes)
    }
    pub fn decode(bytes: &[u8]) -> Result<Self, Error> {
        if bytes.len() > MAX_BYTES {
            return Err(Error::Invalid);
        }
        let intent: Self = serde_json::from_slice(bytes).map_err(|_| Error::Invalid)?;
        intent.validate()?;
        Ok(intent)
    }
    /// Canonical typed encoding, independent of incoming JSON key order. Not a
    /// signature: authenticated callers and independently stored scope are needed.
    pub fn digest(&self) -> Result<[u8; 32], Error> {
        Ok(Sha256::digest(self.encode()?).into())
    }
}

/// Renewable delivery envelope; none of these fields can alter frozen scope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub intent: Intent,
    pub reporting_epoch: i64,
    pub revision: i64,
    pub expires_unix_ms: i64,
}
impl Request {
    /// `retained` and `current_epoch` must come from independent durable state;
    /// `now_unix_ms` is the receiver's clock. This validates no cleanup evidence.
    pub fn validate(
        &self,
        retained: &Intent,
        current_epoch: i64,
        minimum_revision: i64,
        now_unix_ms: i64,
    ) -> Result<(), Error> {
        self.intent.validate()?;
        retained.validate()?;
        if &self.intent != retained {
            return Err(Error::Scope);
        }
        if minimum_revision <= 0
            || self.revision < minimum_revision
            || now_unix_ms <= 0
            || self.expires_unix_ms <= now_unix_ms
            || self.reporting_epoch < self.intent.permit.original_epoch
            || self.reporting_epoch != current_epoch
        {
            return Err(Error::Claim);
        }
        Ok(())
    }
    pub fn encode(&self) -> Result<Vec<u8>, Error> {
        self.intent.validate()?;
        let bytes = serde_json::to_vec(self).map_err(|_| Error::Invalid)?;
        if bytes.len() > MAX_BYTES {
            return Err(Error::Invalid);
        }
        Ok(bytes)
    }
    /// Parsing alone cannot validate a claim against independently retained state.
    pub fn decode(bytes: &[u8]) -> Result<Self, Error> {
        if bytes.len() > MAX_BYTES {
            return Err(Error::Invalid);
        }
        let request: Self = serde_json::from_slice(bytes).map_err(|_| Error::Invalid)?;
        request.intent.validate()?;
        Ok(request)
    }
}

#[cfg(test)]
#[path = "allocation_retirement_tests.rs"]
mod tests;

/// A fresh forgetting claim plus the historical metadata request whose database
/// completion the authenticated controller has retained. The envelope is not an
/// independent database receipt; the issuer must check that completion first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ForgetRequest {
    pub version: u32,
    pub claim: Request,
    pub metadata_request: Request,
}
impl ForgetRequest {
    fn shape(&self) -> Result<(), Error> {
        self.claim.intent.validate()?;
        self.metadata_request.intent.validate()?;
        if self.version != 1
            || self.claim.intent.simulated
            || self.claim.intent != self.metadata_request.intent
            || self.metadata_request.revision <= 0
            || self.metadata_request.expires_unix_ms <= 0
            || self.metadata_request.reporting_epoch < self.claim.intent.permit.original_epoch
            || self.metadata_request.reporting_epoch > self.claim.reporting_epoch
            || self.claim.revision <= 0
            || self.claim.expires_unix_ms <= 0
        {
            return Err(Error::Invalid);
        }
        Ok(())
    }
    pub fn validate(&self, epoch: i64, now: i64) -> Result<(), Error> {
        self.shape()?;
        self.claim
            .validate(&self.metadata_request.intent, epoch, 1, now)
    }
    pub fn encode(&self) -> Result<Vec<u8>, Error> {
        self.shape()?;
        let bytes = serde_json::to_vec(self).map_err(|_| Error::Invalid)?;
        if bytes.len() > MAX_BYTES {
            return Err(Error::Invalid);
        }
        Ok(bytes)
    }
    pub fn decode(bytes: &[u8]) -> Result<Self, Error> {
        if bytes.len() > MAX_BYTES {
            return Err(Error::Invalid);
        }
        let r: Self = serde_json::from_slice(bytes).map_err(|_| Error::Invalid)?;
        r.shape()?;
        Ok(r)
    }
}
