//! Bounded private output reads. Reauthorize after storage before releasing bytes.
use crate::{auth::Authenticated, problem::Problem, reads::no_store};
use axum::{
    Router,
    extract::{
        FromRef, Path, Query, State,
        rejection::{PathRejection, QueryRejection},
    },
    response::{IntoResponse, Response},
    routing::get,
};
use http::{HeaderMap, header};
use sandbox_artifacts::{ArtifactStore, Error, OutputChunk};
use sandbox_protocol::{
    OperationId,
    output::{MAX_CHUNK, OutputName, OutputOwner, OutputRef},
};
use sandbox_store::{Store, output::OutputView};
use serde::Deserialize;
use std::{fmt::Debug, future::Future, pin::Pin, sync::Arc, time::Duration};
use time::OffsetDateTime;
use tokio::sync::Semaphore;

/// Trusted service adapter: implementations must verify the complete object's
/// identity, retention, length and integrity before returning the requested range.
/// The production adapter is ArtifactStore; this interface grants no write access.
pub trait OutputReader: Debug + Send + Sync {
    fn read<'a>(
        &'a self,
        reference: &'a OutputRef,
        owner: &'a OutputOwner,
        now: i64,
        offset: u64,
        limit: usize,
    ) -> Pin<Box<dyn Future<Output = Result<OutputChunk, Error>> + Send + 'a>>;
}
impl OutputReader for ArtifactStore {
    fn read<'a>(
        &'a self,
        reference: &'a OutputRef,
        owner: &'a OutputOwner,
        now: i64,
        offset: u64,
        limit: usize,
    ) -> Pin<Box<dyn Future<Output = Result<OutputChunk, Error>> + Send + 'a>> {
        Box::pin(ArtifactStore::read(
            self, reference, owner, now, offset, limit,
        ))
    }
}
static READS: Semaphore = Semaphore::const_new(4);
const READ_TIMEOUT: Duration = Duration::from_secs(25);
#[derive(Debug, Clone)]
struct OutputState {
    store: Store,
    reader: Option<Arc<dyn OutputReader>>,
}
impl FromRef<OutputState> for Store {
    fn from_ref(state: &OutputState) -> Store {
        state.store.clone()
    }
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadQuery {
    offset: Option<u64>,
    limit: Option<usize>,
}
fn now() -> Result<i64, Problem> {
    i64::try_from(OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000)
        .map_err(|_| Problem::Unavailable)
}
fn selected(
    view: OutputView,
    name: OutputName,
) -> Result<(OutputOwner, OutputRef, bool, OffsetDateTime), Problem> {
    match view.status.as_str() {
        "expired" => return Err(Problem::OutputExpired),
        "none" | "pending" | "uploading" => return Err(Problem::OutputNotReady),
        "published" => {}
        _ => return Err(Problem::Unavailable),
    }
    let simulated = view.simulated.ok_or(Problem::Unavailable)?;
    let owner = view.owner.ok_or(Problem::Unavailable)?;
    let refs = view.references.ok_or(Problem::Unavailable)?;
    Ok((
        owner,
        match name {
            OutputName::Stdout => refs.stdout,
            OutputName::Stderr => refs.stderr,
        },
        simulated,
        view.expires_at.ok_or(Problem::Unavailable)?,
    ))
}
fn storage_error(error: Error) -> Problem {
    match error {
        Error::Missing => Problem::OutputMissing,
        Error::Expired => Problem::OutputExpired,
        Error::Corrupt | Error::Conflict => Problem::OutputCorrupt,
        Error::Bounds => Problem::OutputRange,
        _ => Problem::Unavailable,
    }
}
async fn read(
    State(state): State<OutputState>,
    caller: Authenticated,
    path: Result<Path<(String, String)>, PathRejection>,
    query: Result<Query<ReadQuery>, QueryRejection>,
    headers: HeaderMap,
) -> Result<Response, Problem> {
    let Path((operation, name)) = path.map_err(|_| Problem::BadRequest("invalid output path"))?;
    let operation: OperationId = operation
        .parse()
        .map_err(|_| Problem::BadRequest("invalid operation id"))?;
    let name = match name.as_str() {
        "stdout" => OutputName::Stdout,
        "stderr" => OutputName::Stderr,
        _ => return Err(Problem::NotFound),
    };
    let Query(query) = query.map_err(|_| Problem::BadRequest("invalid output query"))?;
    let offset = query.offset.unwrap_or(0);
    let limit = query.limit.unwrap_or(MAX_CHUNK);
    if limit == 0 || limit > MAX_CHUNK || headers.contains_key(header::RANGE) {
        return Err(Problem::BadRequest(
            "use offset and limit between 1 and 32768; Range is unsupported",
        ));
    }
    let op = state
        .store
        .operation_for_project(caller.project_id, operation)
        .await
        .map_err(|_| Problem::Unavailable)?
        .ok_or(Problem::NotFound)?;
    if op.kind != "execute" {
        return Err(Problem::NotFound);
    }
    let view = state
        .store
        .output_for_project(caller.project_id, operation)
        .await
        .map_err(|_| Problem::Unavailable)?
        .ok_or(Problem::NotFound)?;
    let (owner, reference, simulated, _) = selected(view, name)?;
    if offset > reference.plan.size {
        return Err(Problem::OutputRange);
    }
    let reader = state.reader.as_ref().ok_or(Problem::Unavailable)?;
    let _permit = READS.try_acquire().map_err(|_| Problem::Unavailable)?;
    let loaded = tokio::time::timeout(
        READ_TIMEOUT,
        reader.read(&reference, &owner, now()?, offset, limit),
    )
    .await;
    // Fail closed even when storage returned an error. Never disclose bytes or
    // object availability based on credentials checked before a slow transfer.
    let latest = state
        .store
        .output_for_project(caller.project_id, operation)
        .await;
    // This must be the final awaited check: the metadata query can also stall.
    caller.revalidate(&state.store).await?;
    let latest = latest
        .map_err(|_| Problem::Unavailable)?
        .ok_or(Problem::NotFound)?;
    let (current_owner, current_ref, current_simulated, expires_at) = selected(latest, name)?;
    if current_owner != owner || current_ref != reference || current_simulated != simulated {
        return Err(Problem::Unavailable);
    }
    if now()? >= reference.plan.expires_unix_ms || OffsetDateTime::now_utc() >= expires_at {
        return Err(Problem::OutputExpired);
    }
    let chunk = loaded
        .map_err(|_| Problem::Unavailable)?
        .map_err(storage_error)?;
    let expected_size = (reference.plan.size - offset).min(limit as u64);
    if chunk.bytes.len() as u64 != expected_size
        || chunk.next_offset != offset + expected_size
        || chunk.eof != (chunk.next_offset == reference.plan.size)
        || chunk.truncated != reference.plan.truncated
    {
        return Err(Problem::OutputCorrupt);
    }
    let mut response = no_store(chunk.bytes.into_response());
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("application/octet-stream"),
    );
    headers.insert(
        header::CONTENT_DISPOSITION,
        header::HeaderValue::from_static(match name {
            OutputName::Stdout => "attachment; filename=stdout.bin",
            OutputName::Stderr => "attachment; filename=stderr.bin",
        }),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        header::HeaderValue::from_static("nosniff"),
    );
    for (key, value) in [
        ("x-output-simulated", simulated.to_string()),
        ("x-output-offset", offset.to_string()),
        ("x-output-next-offset", chunk.next_offset.to_string()),
        ("x-output-size", reference.plan.size.to_string()),
        ("x-output-seen", reference.plan.seen.to_string()),
        ("x-output-eof", chunk.eof.to_string()),
        ("x-output-truncated", chunk.truncated.to_string()),
    ] {
        headers.insert(key, value.parse().map_err(|_| Problem::Internal)?);
    }
    Ok(response)
}
pub(crate) fn routes(store: Store, reader: Option<Arc<dyn OutputReader>>) -> Router {
    Router::new()
        .route(
            "/v1/operations/{operation_id}/outputs/{output_name}",
            get(read),
        )
        .with_state(OutputState { store, reader })
}
