//! Normalized command input shared by admission and dispatch. Values never enter Debug.
pub use crate::api::CommandInput;
use crate::{Id, OperationId, guest_model::Execute};
use serde::{Deserialize, Serialize};

pub const MAX_OUTPUT: u64 = 10 * 1024 * 1024;
pub const MAX_DURATION_MS: i64 = 6 * 60 * 60 * 1000;
impl CommandInput {
    pub fn for_operation(&self, operation_id: OperationId) -> Execute {
        Execute {
            operation_id,
            argv: self.argv.clone(),
            env: self.env.clone(),
            cwd: self.cwd.clone(),
            deadline_unix_ms: self.deadline_unix_ms,
            output_limit: self.output_limit,
        }
    }
    /// Shape only. Database time checks a new deadline after idempotency lookup;
    /// retries retain their original handle even after the command deadline.
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.output_limit <= MAX_OUTPUT,
            "output limit exceeds public cap"
        );
        self.for_operation(OperationId::generate()).validate()
    }
}

/// Leaves space in each allocation's 64-operation fence budget for lifecycle work.
pub const MAX_COMMANDS: usize = 32;

/// Host-owned command evidence. Persisted before forwarding any guest Execute.
/// No command arguments or environment are retained in this host journal.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandRecord {
    pub digest: [u8; 32],
    pub context: Option<crate::guest_model::Context>,
    pub deadline_unix_ms: i64,
    pub output_limit: u64,
    pub not_started: bool,
    pub receipt: Option<crate::guest_model::Receipt>,
}
impl CommandRecord {
    pub fn fenced(digest: [u8; 32]) -> Self {
        Self {
            digest,
            context: None,
            deadline_unix_ms: 0,
            output_limit: 0,
            not_started: true,
            receipt: None,
        }
    }
    pub fn pending(
        command: &Execute,
        context: crate::guest_model::Context,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            digest: command.digest()?,
            context: Some(context),
            deadline_unix_ms: command.deadline_unix_ms,
            output_limit: command.output_limit,
            not_started: false,
            receipt: None,
        })
    }
    pub fn finished(&self) -> bool {
        self.not_started
            || self.receipt.as_ref().is_some_and(|r| {
                r.cleanup_confirmed
                    && matches!(
                        r.state,
                        crate::guest_model::State::Exited
                            | crate::guest_model::State::TimedOut
                            | crate::guest_model::State::Cancelled
                    )
            })
    }
    pub fn validate_receipt(
        &self,
        id: OperationId,
        receipt: &crate::guest_model::Receipt,
    ) -> anyhow::Result<()> {
        receipt.validate()?;
        anyhow::ensure!(
            !self.not_started
                && receipt.operation_id == id
                && self.context.as_ref() == Some(&receipt.context)
                && self.digest == receipt.digest
                && self.deadline_unix_ms == receipt.deadline_unix_ms
                && self.output_limit == receipt.output_limit,
            "command receipt does not match retained intent"
        );
        Ok(())
    }
    pub fn observation(
        &self,
        owner: crate::supervisor::Ownership,
        simulated: bool,
        now: i64,
    ) -> crate::supervisor::CommandObservation {
        crate::supervisor::CommandObservation {
            ownership: Some(owner),
            simulated,
            observed_unix_ms: now,
            command_digest: self.digest.to_vec(),
            receipt: self.receipt.as_ref().map(Into::into),
            not_started: self.not_started,
        }
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    use crate::RequestDigest;
    use serde_json::json;
    #[test]
    fn default_fields_and_environment_order_have_one_digest() {
        let first: CommandInput = serde_json::from_value(
            json!({"argv":["/bin/echo"],"deadline_unix_ms":1,"env":{"Z":"last","A":"first"}}),
        )
        .expect("command");
        let second: CommandInput=serde_json::from_value(json!({"argv":["/bin/echo"],"deadline_unix_ms":1,"cwd":"/","output_limit":1048576,"env":{"A":"first","Z":"last"}})).expect("command");
        assert_eq!(
            RequestDigest::compute("POST", "/execute", &first).expect("digest"),
            RequestDigest::compute("POST", "/execute", &second).expect("digest")
        );
        assert!(!format!("{first:?}").contains("/bin/echo"));
        assert!(
            serde_json::from_value::<CommandInput>(
                json!({"argv":["/bin/echo"],"deadline_unix_ms":1,"secret":"no"})
            )
            .is_err()
        );
    }
}
