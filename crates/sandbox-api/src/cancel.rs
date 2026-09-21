//! Cancellation requests are durable operations, not an HTTP disconnect hook.
use crate::{
    AppState, auth::Authenticated, headers::RequestKey, problem::Problem, sandboxes::accepted,
};
use axum::{
    Json,
    extract::{Path, State},
    response::Response,
};
use sandbox_protocol::OperationId;
use sandbox_store::cancel::{CancelAdmission, CancelCommand};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CancelRequest {}

async fn cancel(
    State(state): State<AppState>,
    caller: Authenticated,
    Path(id): Path<String>,
    RequestKey(key): RequestKey,
    request: Result<Json<CancelRequest>, axum::extract::rejection::JsonRejection>,
) -> Result<Response, Problem> {
    let Json(_) = request.map_err(Problem::from_json)?;
    let target: OperationId = id
        .parse()
        .map_err(|_| Problem::BadRequest("invalid operation id"))?;
    match state
        .store
        .admit_cancel(&CancelCommand {
            project_id: caller.project_id,
            target,
            key_id: caller.key_id,
            idempotency_key: key,
        })
        .await
        .map_err(|_| Problem::Unavailable)?
    {
        CancelAdmission::Accepted {
            operation_id,
            sandbox_id,
            status,
        } => Ok(accepted(
            &sandbox_id.to_string(),
            &operation_id.to_string(),
            &status,
        )),
        CancelAdmission::ResponseExpired(id) => Err(Problem::ResponseExpired(id)),
        CancelAdmission::Unauthorized => Err(Problem::Unauthenticated),
        CancelAdmission::NotFound => Err(Problem::NotFound),
        CancelAdmission::Unsupported => Err(Problem::Conflict(
            "only owned execute operations support cancellation",
        )),
        CancelAdmission::DigestConflict => Err(Problem::Conflict(
            "idempotency key was used for a different request",
        )),
        CancelAdmission::Busy(id) => Err(Problem::CommandInProgress(id)),
    }
}
pub fn routes() -> axum::Router<AppState> {
    axum::Router::new().route(
        "/v1/operations/{operation_id}/cancel",
        axum::routing::post(cancel),
    )
}
