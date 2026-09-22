//! Status routes.
//!
//! Reading an operation is how a caller learns what happened, so these are the
//! routes that make `202 Accepted` usable. Acceptance is not completion: the
//! response here distinguishes what was intended from what has been confirmed,
//! and never presents one as the other.

use axum::Json;
use axum::extract::{Path, State};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use http::{StatusCode, header};
pub use sandbox_protocol::api::{OperationBody, SandboxBody};
use sandbox_protocol::{OperationId, SandboxId};
use sandbox_store::reads::{OperationView, SandboxView};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::AppState;
use crate::auth::Authenticated;
use crate::problem::Problem;

/// Format a timestamp, or report the bug rather than inventing one.
fn stamp(at: OffsetDateTime) -> Result<String, Problem> {
    at.format(&Rfc3339).map_err(|error| {
        tracing::error!(%error, "could not format a stored timestamp");
        Problem::Internal
    })
}

/// `GET /v1/operations/{operation_id}`.
pub async fn operation(
    State(state): State<AppState>,
    caller: Authenticated,
    path: Result<Path<String>, axum::extract::rejection::PathRejection>,
) -> Result<Response, Problem> {
    let Path(operation_id) = path.map_err(|_| Problem::BadRequest("invalid operation id"))?;
    // A malformed id is a 400: the caller can fix it, and saying so does not
    // reveal whether any such operation exists.
    let operation_id: OperationId = operation_id
        .parse()
        .map_err(|_| Problem::BadRequest("that is not a valid operation id"))?;

    let found = state
        .store
        .operation_for_project(caller.project_id, operation_id)
        .await
        .map_err(|error| {
            tracing::error!(%error, "reading an operation failed");
            Problem::Unavailable
        })?;

    let view = found.ok_or(Problem::NotFound)?;
    let id = view.id;
    let body = body_for(view)?;
    if body.response_expired {
        return Err(Problem::ResponseExpired(id));
    }
    Ok(no_store(Json(body)))
}

pub(crate) fn body_for(view: OperationView) -> Result<OperationBody, Problem> {
    let expired = view.response_expired();
    Ok(OperationBody {
        response_expired: expired,
        operation_id: view.id.to_string(),
        sandbox_id: view.sandbox_id.to_string(),
        target_operation_id: view.target_operation_id.map(|id| id.to_string()),
        kind: view.kind,
        status: view.status,
        phase: view.phase,
        output_status: view.output_status,
        result: if expired { None } else { view.result },
        error: if expired { None } else { view.error },
        created_at: stamp(view.created_at)?,
        completed_at: view.completed_at.map(stamp).transpose()?,
    })
}

/// `GET /v1/sandboxes/{sandbox_id}`.
pub async fn sandbox(
    State(state): State<AppState>,
    caller: Authenticated,
    path: Result<Path<String>, axum::extract::rejection::PathRejection>,
) -> Result<Response, Problem> {
    let Path(sandbox_id) = path.map_err(|_| Problem::BadRequest("invalid sandbox id"))?;
    let sandbox_id: SandboxId = sandbox_id
        .parse()
        .map_err(|_| Problem::BadRequest("that is not a valid sandbox id"))?;

    let found = state
        .store
        .sandbox_for_project(caller.project_id, sandbox_id)
        .await
        .map_err(|error| {
            tracing::error!(%error, "reading a sandbox failed");
            Problem::Unavailable
        })?;

    let view = found.ok_or(Problem::NotFound)?;
    Ok(no_store(Json(sandbox_body_for(view)?)))
}

pub(crate) fn sandbox_body_for(view: SandboxView) -> Result<SandboxBody, Problem> {
    Ok(SandboxBody {
        observation_simulated: view.observation_simulated,
        sandbox_id: view.id.to_string(),
        name: view.name,
        desired_state: view.desired_state,
        observed_state: view.observed_state,
        observed_at: view.observed_at.map(stamp).transpose()?,
        image_digest: view.image_digest,
        resources: view.resources,
        generation: view.generation,
        active_operation_id: view.active_transition_operation_id.map(|id| id.to_string()),
        created_at: stamp(view.created_at)?,
    })
}

/// Status and list responses must not be cached anywhere in between.
pub(crate) fn no_store<T: IntoResponse>(body: T) -> Response {
    let mut response = (StatusCode::OK, body).into_response();
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static("no-store"),
    );
    response
}

/// Build the status routes.
pub fn routes() -> axum::Router<AppState> {
    axum::Router::new()
        .route("/v1/operations/{operation_id}", get(operation))
        .route("/v1/sandboxes/{sandbox_id}", get(sandbox))
}
