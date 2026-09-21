//! Exact read scope, derived by a trusted API from authorized execution evidence.
//! This descriptor is not a customer credential or permission to execute.
use crate::{
    command::MAX_OUTPUT,
    output::{InvalidOutput, OutputOwner},
    supervisor::{LiveOutputObservation, LiveOutputRequest},
};
use serde::{Deserialize, Serialize};
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LiveOutputScope {
    pub version: u32,
    pub owner: OutputOwner,
    pub command_digest: [u8; 32],
    pub output_limit: u64,
    pub deadline_unix_ms: i64,
}
impl LiveOutputScope {
    pub fn validate(&self) -> Result<(), InvalidOutput> {
        self.owner.validate()?;
        if self.version != 1
            || self.deadline_unix_ms <= 0
            || !(1..=MAX_OUTPUT).contains(&self.output_limit)
        {
            return Err(InvalidOutput);
        }
        Ok(())
    }
}
impl std::fmt::Debug for LiveOutputRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LiveOutputRequest")
            .field("expires_unix_ms", &self.expires_unix_ms)
            .finish_non_exhaustive()
    }
}
impl std::fmt::Debug for LiveOutputObservation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LiveOutputObservation")
            .field("host_id", &self.host_id)
            .field("supervisor_epoch", &self.supervisor_epoch)
            .field("simulated", &self.simulated)
            .finish_non_exhaustive()
    }
}
