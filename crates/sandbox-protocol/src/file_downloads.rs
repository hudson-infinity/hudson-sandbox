//! Allocation read scope. Derived by an authorized service, never a customer credential.
use crate::{
    AllocationId, HostId, OperationId, ProjectId, SandboxId, file_wire, files,
    guest_model::Context, supervisor::*,
};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadScope {
    pub version: u32,
    pub host_id: HostId,
    pub host_epoch: i64,
    pub project_id: ProjectId,
    pub sandbox_id: SandboxId,
    pub allocation_id: AllocationId,
    pub generation: i64,
}
impl ReadScope {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.version == 1 && self.host_epoch > 0 && self.generation > 0,
            "invalid file read scope"
        );
        Ok(())
    }
    pub fn matches_owner(&self, o: &Ownership) -> bool {
        self.host_id.to_string() == o.host_id
            && self.host_epoch == o.supervisor_epoch
            && self.project_id.to_string() == o.project_id
            && self.sandbox_id.to_string() == o.sandbox_id
            && self.allocation_id.to_string() == o.allocation_id
            && self.generation == o.generation
    }
}
pub fn scope(bytes: &[u8], expires: i64, now: i64) -> Result<ReadScope> {
    ensure!(
        bytes.len() <= 4096 && (1..=30_000).contains(&expires.saturating_sub(now)),
        "invalid file read deadline or scope"
    );
    let scope: ReadScope = serde_json::from_slice(bytes)?;
    scope.validate()?;
    Ok(scope)
}
pub fn handle(h: &FileDownloadHandle, s: &ReadScope) -> Result<()> {
    h.id.parse::<OperationId>()?;
    let c: Context = h
        .context
        .clone()
        .ok_or_else(|| anyhow::anyhow!("missing context"))?
        .try_into()?;
    ensure!(
        c.allocation_id == s.allocation_id
            && c.generation == s.generation
            && !c.boot_id.is_empty()
            && c.boot_id.len() <= 64
            && h.expires_unix_ms > 0,
        "invalid capture ownership"
    );
    file_wire::validate_capture(
        h.capture
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("missing capture"))?,
    )
}
pub fn capture_request(r: &FileCaptureRequest, now: i64) -> Result<ReadScope> {
    let s = scope(&r.scope_json, r.expires_unix_ms, now)?;
    files::validate_path(&r.path)?;
    Ok(s)
}
pub fn read_request(r: &FileDownloadRequest, now: i64) -> Result<ReadScope> {
    let s = scope(&r.scope_json, r.expires_unix_ms, now)?;
    let h = r
        .handle
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("missing handle"))?;
    handle(h, &s)?;
    ensure!(
        (1..=files::MAX_CHUNK_BYTES as u32).contains(&r.limit)
            && r.offset
                <= h.capture
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("missing capture"))?
                    .size,
        "invalid capture range"
    );
    Ok(s)
}
pub fn release_request(r: &FileReleaseRequest, now: i64) -> Result<ReadScope> {
    let s = scope(&r.scope_json, r.expires_unix_ms, now)?;
    handle(
        r.handle
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("missing handle"))?,
        &s,
    )?;
    Ok(s)
}
// Requests and responses may contain customer paths, digests or bytes. Keep Debug structural.
macro_rules! redacted {
    ($($name:ident),+ $(,)?) => { $(impl std::fmt::Debug for $name {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct(stringify!($name)).finish_non_exhaustive()
        }
    })+ };
}
redacted!(
    FileDownloadHandle,
    FileCaptureRequest,
    FileDownloadRequest,
    FileReleaseRequest,
    FileAccessObservation
);

impl std::fmt::Debug for file_access_observation::Result {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            Self::Captured(_) => "Captured",
            Self::Chunk(_) => "Chunk",
            Self::Released(_) => "Released",
        };
        f.debug_struct(name).finish_non_exhaustive()
    }
}

fn observation(scope: &[u8], o: &FileAccessObservation, simulated: bool, now: i64) -> Result<()> {
    ensure!(
        o.scope_json == scope
            && o.simulated == simulated
            && o.observed_unix_ms > 0
            && o.observed_unix_ms.abs_diff(now) <= 10_000,
        "file response scope, provenance or time changed"
    );
    Ok(())
}
pub fn captured(
    r: &FileCaptureRequest,
    o: &FileAccessObservation,
    simulated: bool,
    now: i64,
) -> Result<FileDownloadHandle> {
    let scope = capture_request(r, now)?;
    observation(&r.scope_json, o, simulated, now)?;
    let Some(file_access_observation::Result::Captured(h)) = &o.result else {
        anyhow::bail!("capture response required");
    };
    handle(h, &scope)?;
    ensure!(
        h.capture.as_ref().is_some_and(|c| c.path == r.path)
            && h.expires_unix_ms > now
            && h.expires_unix_ms <= o.observed_unix_ms.saturating_add(60_000),
        "capture identity or expiry changed"
    );
    Ok(h.clone())
}
pub fn chunk(
    r: &FileDownloadRequest,
    o: &FileAccessObservation,
    simulated: bool,
    now: i64,
) -> Result<crate::guest::FileChunk> {
    read_request(r, now)?;
    observation(&r.scope_json, o, simulated, now)?;
    let capture = r
        .handle
        .as_ref()
        .and_then(|h| h.capture.as_ref())
        .ok_or_else(|| anyhow::anyhow!("capture required"))?;
    let Some(file_access_observation::Result::Chunk(c)) = &o.result else {
        anyhow::bail!("chunk response required");
    };
    let expected = (capture.size - r.offset).min(r.limit as u64);
    let next = r.offset + expected;
    ensure!(
        c.handle.as_ref() == Some(&file_wire::handle(capture)?)
            && c.offset == r.offset
            && c.size == capture.size
            && c.data.len() as u64 == expected
            && c.next_offset == next
            && c.at_end == (next == capture.size),
        "file chunk identity or range changed"
    );
    Ok(c.clone())
}
pub fn released(
    r: &FileReleaseRequest,
    o: &FileAccessObservation,
    simulated: bool,
    now: i64,
) -> Result<()> {
    release_request(r, now)?;
    observation(&r.scope_json, o, simulated, now)?;
    let Some(file_access_observation::Result::Released(h)) = &o.result else {
        anyhow::bail!("release response required");
    };
    ensure!(Some(h) == r.handle.as_ref(), "release handle changed");
    Ok(())
}
