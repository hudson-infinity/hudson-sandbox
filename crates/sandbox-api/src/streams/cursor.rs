use crate::problem::Problem;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use sandbox_protocol::{OperationId, ProjectId, command::MAX_OUTPUT, output::OutputOwner};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Cursor {
    version: u32,
    project: ProjectId,
    operation: OperationId,
    binding: String,
    pub(super) offsets: [u64; 2],
}
impl Cursor {
    pub(super) fn new(owner: &OutputOwner) -> Result<Self, Problem> {
        let bytes = serde_json::to_vec(owner).map_err(|_| Problem::Internal)?;
        Ok(Self {
            version: 1,
            project: owner.project_id,
            operation: owner.operation_id,
            binding: hex::encode(Sha256::digest(bytes)),
            offsets: [0, 0],
        })
    }
    pub(super) fn parse(raw: &str, owner: &OutputOwner) -> Result<Self, Problem> {
        if raw.len() > 2048 {
            return Err(Problem::BadRequest("invalid stream cursor"));
        }
        let c: Self = URL_SAFE_NO_PAD
            .decode(raw)
            .ok()
            .and_then(|v| serde_json::from_slice(&v).ok())
            .ok_or(Problem::BadRequest("invalid stream cursor"))?;
        let expected = Self::new(owner)?;
        if c.version != 1
            || c.project != owner.project_id
            || c.operation != owner.operation_id
            || c.offsets[0]
                .checked_add(c.offsets[1])
                .is_none_or(|sum| sum > MAX_OUTPUT)
        {
            return Err(Problem::BadRequest("invalid stream cursor"));
        }
        if c.binding != expected.binding {
            return Err(Problem::OutputMissing);
        }
        Ok(c)
    }
    pub(super) fn encode(&self) -> Result<String, Problem> {
        Ok(URL_SAFE_NO_PAD.encode(serde_json::to_vec(self).map_err(|_| Problem::Internal)?))
    }
    pub(super) fn matches(&self, owner: &OutputOwner) -> Result<bool, Problem> {
        Ok(self.binding == Self::new(owner)?.binding)
    }
}
