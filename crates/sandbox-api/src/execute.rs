//! Tenant-scoped command admission. HTTP lifetime does not own the guest process.
use crate::{
    AppState, auth::Authenticated, headers::RequestKey, problem::Problem, sandboxes::accepted,
};
use axum::{
    Json,
    extract::{Path, State},
    response::Response,
};
use sandbox_protocol::{SandboxId, command::CommandInput};
use sandbox_store::execute::{ExecuteAdmission, ExecuteCommand};

pub async fn execute(
    State(state): State<AppState>,
    caller: Authenticated,
    Path(id): Path<String>,
    RequestKey(key): RequestKey,
    request: Result<Json<CommandInput>, axum::extract::rejection::JsonRejection>,
) -> Result<Response, Problem> {
    let Json(command) = request.map_err(Problem::from_json)?;
    let sandbox: SandboxId = id
        .parse()
        .map_err(|_| Problem::BadRequest("invalid sandbox id"))?;
    let result = state
        .store
        .admit_execute(&ExecuteCommand {
            project_id: caller.project_id,
            sandbox_id: sandbox,
            key_id: caller.key_id,
            idempotency_key: key,
            command,
        })
        .await
        .map_err(|_| Problem::Unavailable)?;
    match result {
        ExecuteAdmission::Accepted {
            operation_id,
            status,
        } => Ok(accepted(
            &sandbox.to_string(),
            &operation_id.to_string(),
            &status,
        )),
        ExecuteAdmission::NotFound => Err(Problem::NotFound),
        ExecuteAdmission::Gone => Err(Problem::Gone),
        ExecuteAdmission::Unauthorized => Err(Problem::Unauthenticated),
        ExecuteAdmission::DigestConflict => Err(Problem::Conflict(
            "idempotency key was used for a different request",
        )),
        ExecuteAdmission::InvalidCommand => Err(Problem::BadRequest(
            "invalid command input or output budget",
        )),
        ExecuteAdmission::InvalidDeadline => Err(Problem::BadRequest(
            "deadline must be within six hours and sandbox lifetime",
        )),
        ExecuteAdmission::NotRunning => Err(Problem::Conflict(
            "sandbox is not running with a current execution lease",
        )),
        ExecuteAdmission::Busy(id) => Err(Problem::CommandInProgress(id)),
    }
}
pub fn routes() -> axum::Router<AppState> {
    axum::Router::new().route(
        "/v1/sandboxes/{sandbox_id}/execute",
        axum::routing::post(execute),
    )
}
