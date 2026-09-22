//! Generated from api/openapi.json by scripts/generate_api.py. Do not edit.
//! Wire shapes only; bounds, ownership and lifecycle checks remain in admission.
use serde::{Deserialize, Serialize};
fn required_nullable<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::deserialize(deserializer)
}

#[derive(Debug, Clone, Serialize, Deserialize, Copy)]
pub struct RequestedResources {
    pub vcpu: i32,
    pub memory_mib: i64,
    pub disk_mib: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateRequest {
    pub image_digest: String,
    #[serde(default)]
    pub name: Option<String>,
    pub resources: RequestedResources,
    #[serde(default)]
    pub correlation_id: Option<String>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandInput {
    pub argv: Vec<String>,
    #[serde(default = "command_input_env_default")]
    pub env: std::collections::BTreeMap<String, String>,
    #[serde(default = "command_input_cwd_default")]
    pub cwd: String,
    pub deadline_unix_ms: i64,
    #[serde(default = "command_input_output_limit_default")]
    pub output_limit: u64,
}
fn command_input_env_default() -> std::collections::BTreeMap<String, String> {
    Default::default()
}
fn command_input_cwd_default() -> String {
    "/".into()
}
fn command_input_output_limit_default() -> u64 {
    1048576
}
impl std::fmt::Debug for CommandInput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CommandInput").finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DestroyRequest {
    #[serde(default)]
    pub correlation_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CancelRequest {}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdmittedResponse {
    pub sandbox_id: String,
    pub operation_id: String,
    pub status: String,
    pub status_url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProblemBody {
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub operation_id: Option<String>,
    pub title: String,
    pub status: u16,
    pub code: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OperationBody {
    #[serde(default = "operation_body_response_expired_default")]
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub response_expired: bool,
    pub operation_id: String,
    pub sandbox_id: String,
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_operation_id: Option<String>,
    pub kind: String,
    pub status: String,
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_status: Option<String>,
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<serde_json::Value>,
    pub created_at: String,
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<String>,
}
fn operation_body_response_expired_default() -> bool {
    false
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxBody {
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observation_simulated: Option<bool>,
    pub sandbox_id: String,
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub desired_state: String,
    pub observed_state: String,
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observed_at: Option<String>,
    pub image_digest: String,
    pub resources: serde_json::Value,
    pub generation: i64,
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_operation_id: Option<String>,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxList {
    pub items: Vec<SandboxBody>,
    #[serde(deserialize_with = "required_nullable")]
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OperationList {
    pub items: Vec<OperationBody>,
    #[serde(deserialize_with = "required_nullable")]
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileCaptureRequest {
    pub path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileCaptureResponse {
    pub capture: String,
    pub size: u64,
    pub sha256: String,
    pub expires_unix_ms: i64,
    pub chunk_size: u32,
    pub simulated: bool,
    pub guest_reported: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutputStats {
    pub seen: u64,
    pub stored: u64,
    pub truncated: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutputEvent {
    pub stream: String,
    pub offset: u64,
    pub next_offset: u64,
    pub data_base64: String,
    pub at_end: bool,
    pub complete: bool,
    pub seen: u64,
    pub stored: u64,
    pub truncated: bool,
    pub simulated: bool,
    pub guest_reported: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EndEvent {
    pub reason: String,
    pub stdout: OutputStats,
    pub stderr: OutputStats,
    pub simulated: bool,
    pub guest_reported: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamProblemEvent {
    pub code: String,
}
