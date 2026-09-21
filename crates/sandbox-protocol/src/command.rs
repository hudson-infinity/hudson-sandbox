//! Normalized command input shared by admission and dispatch. Values never enter Debug.
use crate::{Id, OperationId, guest_model::Execute};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, fmt};

pub const MAX_OUTPUT: u64 = 10 * 1024 * 1024;
pub const MAX_DURATION_MS: i64 = 6 * 60 * 60 * 1000;
fn default_cwd() -> String {
    "/".into()
}
fn default_output_limit() -> u64 {
    1024 * 1024
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandInput {
    pub argv: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default = "default_cwd")]
    pub cwd: String,
    pub deadline_unix_ms: i64,
    #[serde(default = "default_output_limit")]
    pub output_limit: u64,
}
impl fmt::Debug for CommandInput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CommandInput").finish_non_exhaustive()
    }
}
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
