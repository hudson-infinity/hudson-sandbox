//! Bounded single-PUT ingestion. Only the controller has guest mutation authority.
use crate::{auth::Authenticated, headers::RequestKey, problem::Problem, sandboxes::accepted};
use axum::{
    Router,
    extract::{
        FromRef, Path, Query, Request, State,
        rejection::{PathRejection, QueryRejection},
    },
    response::Response,
};
use sandbox_artifacts::sources::SourceBackend;
use sandbox_protocol::SandboxId;
use sandbox_store::{
    Store,
    uploads::{UploadAdmission, UploadCommand, UploadInput},
};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{sync::Arc, time::Duration};
#[derive(Clone)]
struct UploadState {
    store: Store,
    sources: Option<Arc<dyn SourceBackend>>,
}
impl FromRef<UploadState> for Store {
    fn from_ref(s: &UploadState) -> Self {
        s.store.clone()
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FileQuery {
    path: String,
}
static INGEST: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(4);
fn one<'a>(headers: &'a http::HeaderMap, key: &str) -> Result<&'a str, Problem> {
    if headers.get_all(key).iter().count() != 1 {
        return Err(Problem::BadRequest(
            "exactly one file metadata header is required",
        ));
    }
    headers
        .get(key)
        .and_then(|v| v.to_str().ok())
        .ok_or(Problem::BadRequest("invalid file metadata header"))
}
async fn put(
    State(s): State<UploadState>,
    caller: Authenticated,
    path: Result<Path<String>, PathRejection>,
    query: Result<Query<FileQuery>, QueryRejection>,
    RequestKey(key): RequestKey,
    request: Request,
) -> Result<Response, Problem> {
    let id: SandboxId = path
        .map_err(|_| Problem::BadRequest("invalid sandbox id"))?
        .0
        .parse()
        .map_err(|_| Problem::BadRequest("invalid sandbox id"))?;
    let Query(query) =
        query.map_err(|_| Problem::BadRequest("one canonical workspace path is required"))?;
    let headers = request.headers();
    one(headers, "idempotency-key")?;
    if one(headers, "content-type")? != "application/octet-stream"
        || headers.contains_key("content-encoding")
        || headers.contains_key("range")
    {
        return Err(Problem::BadRequest("use an unencoded binary file body"));
    }
    let size: u64 = one(headers, "x-file-size")?
        .parse()
        .map_err(|_| Problem::BadRequest("invalid file size"))?;
    if size > sandbox_protocol::files::MAX_FILE_BYTES {
        return Err(Problem::PayloadTooLarge);
    }
    let sha = one(headers, "x-file-sha256")?;
    if sha.len() != 64
        || !sha
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(Problem::BadRequest("SHA-256 must be lowercase hex"));
    }
    let sha256: [u8; 32] = hex::decode(sha)
        .map_err(|_| Problem::BadRequest("invalid SHA-256"))?
        .try_into()
        .map_err(|_| Problem::BadRequest("invalid SHA-256"))?;
    let mode = if headers.contains_key("x-file-mode") {
        match one(headers, "x-file-mode")? {
            "0644" => 0o644,
            "0755" => 0o755,
            _ => return Err(Problem::BadRequest("file mode must be 0644 or 0755")),
        }
    } else {
        0o644
    };
    let input = UploadInput {
        path: query.path,
        size,
        sha256,
        mode,
    };
    if !input.validate() {
        return Err(Problem::BadRequest("invalid workspace file descriptor"));
    }
    let _permit = INGEST.try_acquire().map_err(|_| Problem::Unavailable)?;
    let sources = s.sources.as_ref().ok_or(Problem::Unavailable)?;
    let bytes = tokio::time::timeout(
        Duration::from_secs(10),
        axum::body::to_bytes(
            request.into_body(),
            sandbox_protocol::files::MAX_FILE_BYTES as usize,
        ),
    )
    .await
    .map_err(|_| Problem::Unavailable)?
    .map_err(|_| Problem::PayloadTooLarge)?;
    if bytes.len() as u64 != size || Sha256::digest(&bytes).as_slice() != sha256 {
        return Err(Problem::BadRequest("file length or SHA-256 mismatch"));
    }
    let result = caller
        .admit_upload(
            &s.store,
            UploadCommand {
                project_id: caller.project_id,
                sandbox_id: id,
                key_id: caller.key_id.clone(),
                key,
                input,
            },
        )
        .await?;
    // The request body and admission may have waited while credentials changed.
    caller.revalidate(&s.store).await?;
    match result {
        UploadAdmission::Accepted {
            plan,
            status,
            write_source,
        } => {
            if write_source {
                let now =
                    (time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000) as i64;
                let uploaded = tokio::time::timeout(
                    Duration::from_secs(15),
                    sources.upload(&plan, &plan.owner, now, &bytes),
                )
                .await;
                caller.revalidate(&s.store).await?;
                // Admission already committed. Preserve handles even when source storage is
                // uncertain; the controller reconciles this exact plan and the caller can retry.
                if let Ok(Ok(reference)) = uploaded
                    && (reference.plan != *plan || reference.validate().is_err())
                {
                    return Err(Problem::FileCorrupt);
                }
            }
            Ok(accepted(
                &id.to_string(),
                &plan.upload.operation_id.to_string(),
                &status,
            ))
        }
        UploadAdmission::ResponseExpired(id) => Err(Problem::ResponseExpired(id)),
        UploadAdmission::Unauthorized => Err(Problem::Unauthenticated),
        UploadAdmission::NotFound => Err(Problem::NotFound),
        UploadAdmission::Conflict => Err(Problem::Conflict(
            "file write already active or idempotency payload changed",
        )),
        UploadAdmission::NotRunning => Err(Problem::Conflict("sandbox is not currently writable")),
        UploadAdmission::Capacity => {
            Err(Problem::Conflict("retained file upload capacity exhausted"))
        }
        UploadAdmission::Invalid => Err(Problem::BadRequest("invalid file descriptor")),
    }
}
pub(crate) fn routes(store: Store, sources: Option<Arc<dyn SourceBackend>>) -> Router {
    Router::new()
        .route("/v1/sandboxes/{sandbox_id}/files", axum::routing::put(put))
        .with_state(UploadState { store, sources })
}
