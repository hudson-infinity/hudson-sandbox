//! Public captured file reads. Resolve ownership and reauthorize after every backend call.
pub mod client;
mod ticket;
use crate::{auth::Authenticated, problem::Problem};
use axum::{
    Json, Router,
    extract::{
        DefaultBodyLimit, FromRef, Path, Query, State,
        rejection::{JsonRejection, PathRejection, QueryRejection},
    },
    response::{IntoResponse, Response},
    routing::post,
};
use client::FileReader;
use http::{HeaderMap, StatusCode, header};
use sandbox_protocol::api::{FileCaptureRequest as CaptureBody, FileCaptureResponse};
use sandbox_protocol::{
    SandboxId, file_downloads as model, files as fm,
    supervisor::{FileCaptureRequest, FileDownloadRequest, FileReleaseRequest},
};
use sandbox_store::{Store, files::FileView};
use serde::Deserialize;
use std::{sync::Arc, time::Duration};
use ticket::Ticket;
fn no_store(body: impl IntoResponse) -> Response {
    let mut response = body.into_response();
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static("no-store"),
    );
    response
}
static READS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(4);
const TIMEOUT: Duration = Duration::from_secs(10);
#[derive(Debug, Clone)]
struct FileState {
    store: Store,
    reader: Option<Arc<dyn FileReader>>,
}
impl FromRef<FileState> for Store {
    fn from_ref(s: &FileState) -> Self {
        s.store.clone()
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EmptyQuery {}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Range {
    offset: Option<u64>,
    limit: Option<u32>,
}
fn now() -> Result<i64, Problem> {
    i64::try_from(time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000)
        .map_err(|_| Problem::Unavailable)
}
fn sandbox(path: Result<Path<String>, PathRejection>) -> Result<SandboxId, Problem> {
    path.map_err(|_| Problem::BadRequest("invalid sandbox path"))?
        .0
        .parse()
        .map_err(|_| Problem::BadRequest("invalid sandbox id"))
}
async fn view(s: &FileState, caller: &Authenticated, id: SandboxId) -> Result<FileView, Problem> {
    let row = caller.file_view(&s.store, id).await?;
    if row.expired() {
        return Err(Problem::FileMissing);
    }
    Ok(row)
}
async fn current(
    s: &FileState,
    caller: &Authenticated,
    id: SandboxId,
    original: &FileView,
) -> Result<(), Problem> {
    let latest = view(s, caller, id).await?;
    if !latest.same_binding(original) {
        return Err(Problem::FileMissing);
    }
    Ok(())
}
fn check_time(expires: i64) -> Result<(), Problem> {
    if expires <= now()? {
        Err(Problem::FileMissing)
    } else {
        Ok(())
    }
}
async fn capture(
    State(s): State<FileState>,
    caller: Authenticated,
    path: Result<Path<String>, PathRejection>,
    headers: HeaderMap,
    query: Result<Query<EmptyQuery>, QueryRejection>,
    body: Result<Json<CaptureBody>, JsonRejection>,
) -> Result<Response, Problem> {
    let id = sandbox(path)?;
    query.map_err(|_| Problem::BadRequest("capture does not accept query fields"))?;
    let Json(body) = body.map_err(Problem::from_json)?;
    fm::validate_path(&body.path)
        .map_err(|_| Problem::BadRequest("invalid workspace file path"))?;
    if headers.contains_key(ticket::HEADER) || headers.contains_key(header::RANGE) {
        return Err(Problem::BadRequest(
            "capture does not accept a prior token or range",
        ));
    }
    let _permit = READS.try_acquire().map_err(|_| Problem::Unavailable)?;
    let before = view(&s, &caller, id).await?;
    let reader = s.reader.as_ref().ok_or(Problem::Unavailable)?;
    let request = FileCaptureRequest {
        scope_json: serde_json::to_vec(&before.scope).map_err(|_| Problem::Internal)?,
        path: body.path,
        expires_unix_ms: now()? + 15000,
    };
    let loaded = tokio::time::timeout(
        TIMEOUT,
        reader.capture(before.scope.host_id, request.clone()),
    )
    .await;
    current(&s, &caller, id, &before).await?;
    let reply = loaded.map_err(|_| Problem::Unavailable)??;
    let handle = model::captured(&request, &reply, before.simulated, now()?)
        .map_err(|_| Problem::FileCorrupt)?;
    let capture = handle.capture.as_ref().ok_or(Problem::FileCorrupt)?;
    let response = FileCaptureResponse {
        capture: Ticket::new(&before, handle.clone())?.encode()?,
        size: capture.size,
        sha256: hex::encode(&capture.sha256),
        expires_unix_ms: handle.expires_unix_ms,
        chunk_size: fm::MAX_CHUNK_BYTES as u32,
        simulated: before.simulated,
        guest_reported: true,
    };
    Ok(no_store(
        (StatusCode::CREATED, Json(response)).into_response(),
    ))
}
async fn read(
    State(s): State<FileState>,
    caller: Authenticated,
    path: Result<Path<String>, PathRejection>,
    query: Result<Query<Range>, QueryRejection>,
    headers: HeaderMap,
) -> Result<Response, Problem> {
    let id = sandbox(path)?;
    let Query(range) = query.map_err(|_| Problem::BadRequest("invalid file range query"))?;
    let offset = range.offset.unwrap_or(0);
    let limit = range.limit.unwrap_or(fm::MAX_CHUNK_BYTES as u32);
    if !(1..=fm::MAX_CHUNK_BYTES as u32).contains(&limit) || headers.contains_key(header::RANGE) {
        return Err(Problem::BadRequest(
            "use offset and limit from 1 to 32768; Range is unsupported",
        ));
    }
    let _permit = READS.try_acquire().map_err(|_| Problem::Unavailable)?;
    let before = view(&s, &caller, id).await?;
    let ticket = Ticket::parse(&headers, &before, now()?)?;
    let capture = ticket.handle.capture.as_ref().ok_or(Problem::FileMissing)?;
    if offset > capture.size {
        return Err(Problem::FileRange);
    }
    let request = FileDownloadRequest {
        scope_json: serde_json::to_vec(&before.scope).map_err(|_| Problem::Internal)?,
        handle: Some(ticket.handle.clone()),
        offset,
        limit,
        expires_unix_ms: now()? + 15000,
    };
    let reader = s.reader.as_ref().ok_or(Problem::Unavailable)?;
    let loaded =
        tokio::time::timeout(TIMEOUT, reader.read(before.scope.host_id, request.clone())).await;
    current(&s, &caller, id, &before).await?;
    check_time(ticket.handle.expires_unix_ms)?;
    let reply = loaded.map_err(|_| Problem::Unavailable)??;
    let chunk = model::chunk(&request, &reply, before.simulated, now()?)
        .map_err(|_| Problem::FileCorrupt)?;
    let mut response = no_store(chunk.data.into_response());
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("application/octet-stream"),
    );
    headers.insert(
        header::CONTENT_DISPOSITION,
        header::HeaderValue::from_static("attachment; filename=file.bin"),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        header::HeaderValue::from_static("nosniff"),
    );
    for (key, value) in [
        ("x-file-offset", offset.to_string()),
        ("x-file-next-offset", chunk.next_offset.to_string()),
        ("x-file-size", capture.size.to_string()),
        ("x-file-sha256", hex::encode(&capture.sha256)),
        ("x-file-eof", chunk.at_end.to_string()),
        ("x-file-simulated", before.simulated.to_string()),
        ("x-file-guest-reported", "true".into()),
    ] {
        headers.insert(key, value.parse().map_err(|_| Problem::Internal)?);
    }
    Ok(response)
}
async fn release(
    State(s): State<FileState>,
    caller: Authenticated,
    path: Result<Path<String>, PathRejection>,
    headers: HeaderMap,
    query: Result<Query<EmptyQuery>, QueryRejection>,
) -> Result<Response, Problem> {
    let id = sandbox(path)?;
    query.map_err(|_| Problem::BadRequest("release does not accept query fields"))?;
    if headers.contains_key(header::RANGE) {
        return Err(Problem::BadRequest("release does not accept a range"));
    }
    let _permit = READS.try_acquire().map_err(|_| Problem::Unavailable)?;
    let before = view(&s, &caller, id).await?;
    let ticket = Ticket::parse(&headers, &before, now()?)?;
    let request = FileReleaseRequest {
        scope_json: serde_json::to_vec(&before.scope).map_err(|_| Problem::Internal)?,
        handle: Some(ticket.handle.clone()),
        expires_unix_ms: now()? + 15000,
    };
    let reader = s.reader.as_ref().ok_or(Problem::Unavailable)?;
    let loaded = tokio::time::timeout(
        TIMEOUT,
        reader.release(before.scope.host_id, request.clone()),
    )
    .await;
    current(&s, &caller, id, &before).await?;
    check_time(ticket.handle.expires_unix_ms)?;
    let reply = loaded.map_err(|_| Problem::Unavailable)??;
    model::released(&request, &reply, before.simulated, now()?)
        .map_err(|_| Problem::FileCorrupt)?;
    Ok(no_store(StatusCode::NO_CONTENT.into_response()))
}
pub(crate) fn routes(store: Store, reader: Option<Arc<dyn FileReader>>) -> Router {
    Router::new()
        .route(
            "/v1/sandboxes/{sandbox_id}/files/captures",
            post(capture).get(read).delete(release),
        )
        .layer(DefaultBodyLimit::max(16384))
        .with_state(FileState { store, reader })
}
