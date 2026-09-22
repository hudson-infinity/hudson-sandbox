//! Live collection reads with bounded, project/route/filter-scoped cursors.
use crate::{
    AppState,
    auth::Authenticated,
    problem::Problem,
    reads::{body_for, no_store, sandbox_body_for},
};
use axum::{
    Json,
    extract::{Query, State, rejection::QueryRejection},
    response::Response,
};
use sandbox_protocol::{Id, OperationId, ProjectId, SandboxId};
use sandbox_store::lists::{PageLimit, Position};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

const MAX_CURSOR_BYTES: usize = 2048;
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxQuery {
    limit: Option<u16>,
    cursor: Option<String>,
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperationQuery {
    limit: Option<u16>,
    cursor: Option<String>,
    sandbox_id: Option<SandboxId>,
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Kind {
    Sandboxes,
    Operations,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Cursor {
    version: u8,
    project: ProjectId,
    kind: Kind,
    sandbox: Option<SandboxId>,
    created_micros: i64,
    id: String,
}
fn invalid_cursor() -> Problem {
    Problem::BadRequest("invalid cursor for this collection")
}
fn decode(
    value: Option<&str>,
    project: ProjectId,
    kind: Kind,
    sandbox: Option<SandboxId>,
) -> Result<Option<Position>, Problem> {
    let Some(value) = value else { return Ok(None) };
    if value.len() > MAX_CURSOR_BYTES {
        return Err(invalid_cursor());
    }
    let bytes = hex::decode(value.strip_prefix("v1.").ok_or_else(invalid_cursor)?)
        .map_err(|_| invalid_cursor())?;
    let cursor: Cursor = serde_json::from_slice(&bytes).map_err(|_| invalid_cursor())?;
    if cursor.version != 1
        || cursor.project != project
        || cursor.kind != kind
        || cursor.sandbox != sandbox
    {
        return Err(invalid_cursor());
    }
    let id = match kind {
        Kind::Sandboxes => cursor
            .id
            .parse::<SandboxId>()
            .map_err(|_| invalid_cursor())?
            .uuid(),
        Kind::Operations => cursor
            .id
            .parse::<OperationId>()
            .map_err(|_| invalid_cursor())?
            .uuid(),
    };
    let created_at =
        OffsetDateTime::from_unix_timestamp_nanos(i128::from(cursor.created_micros) * 1000)
            .map_err(|_| invalid_cursor())?;
    Ok(Some(Position { created_at, id }))
}
fn encode(
    position: Option<Position>,
    project: ProjectId,
    kind: Kind,
    sandbox: Option<SandboxId>,
) -> Result<Option<String>, Problem> {
    let Some(position) = position else {
        return Ok(None);
    };
    let id = match kind {
        Kind::Sandboxes => SandboxId::from_uuid(position.id).to_string(),
        Kind::Operations => OperationId::from_uuid(position.id).to_string(),
    };
    let cursor = Cursor {
        version: 1,
        project,
        kind,
        sandbox,
        id,
        created_micros: i64::try_from(position.created_at.unix_timestamp_nanos() / 1000)
            .map_err(|_| Problem::Internal)?,
    };
    let encoded = serde_json::to_vec(&cursor).map_err(|_| Problem::Internal)?;
    Ok(Some(format!("v1.{}", hex::encode(encoded))))
}
fn limit(value: Option<u16>) -> Result<PageLimit, Problem> {
    PageLimit::new(value.unwrap_or(50))
        .ok_or(Problem::BadRequest("limit must be between 1 and 100"))
}
pub async fn sandboxes(
    State(state): State<AppState>,
    caller: Authenticated,
    query: Result<Query<SandboxQuery>, QueryRejection>,
) -> Result<Response, Problem> {
    let Query(query) = query.map_err(|_| Problem::BadRequest("invalid list parameters"))?;
    let limit = limit(query.limit)?;
    let before = decode(
        query.cursor.as_deref(),
        caller.project_id,
        Kind::Sandboxes,
        None,
    )?;
    let page = state
        .store
        .list_sandboxes(caller.project_id, before, limit)
        .await
        .map_err(|error| {
            tracing::error!(%error,"listing sandboxes failed");
            Problem::Unavailable
        })?;
    let next_cursor = encode(page.next, caller.project_id, Kind::Sandboxes, None)?;
    let items = page
        .items
        .into_iter()
        .map(sandbox_body_for)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(no_store(Json(sandbox_protocol::api::SandboxList {
        items,
        next_cursor,
    })))
}
pub async fn operations(
    State(state): State<AppState>,
    caller: Authenticated,
    query: Result<Query<OperationQuery>, QueryRejection>,
) -> Result<Response, Problem> {
    let Query(query) = query.map_err(|_| Problem::BadRequest("invalid list parameters"))?;
    let limit = limit(query.limit)?;
    let before = decode(
        query.cursor.as_deref(),
        caller.project_id,
        Kind::Operations,
        query.sandbox_id,
    )?;
    if let Some(sandbox) = query.sandbox_id {
        let found = state
            .store
            .sandbox_for_project(caller.project_id, sandbox)
            .await
            .map_err(|error| {
                tracing::error!(%error,"reading operation filter failed");
                Problem::Unavailable
            })?;
        if found.is_none() {
            return Err(Problem::NotFound);
        }
    }
    let page = state
        .store
        .list_operations(caller.project_id, query.sandbox_id, before, limit)
        .await
        .map_err(|error| {
            tracing::error!(%error,"listing operations failed");
            Problem::Unavailable
        })?;
    let next_cursor = encode(
        page.next,
        caller.project_id,
        Kind::Operations,
        query.sandbox_id,
    )?;
    let items = page
        .items
        .into_iter()
        .map(body_for)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(no_store(Json(sandbox_protocol::api::OperationList {
        items,
        next_cursor,
    })))
}
pub fn routes() -> axum::Router<AppState> {
    axum::Router::new()
        .route("/v1/sandboxes", axum::routing::get(sandboxes))
        .route("/v1/operations", axum::routing::get(operations))
}
