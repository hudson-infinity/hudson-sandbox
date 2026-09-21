//! Strict conversion and bounds for untrusted guest file messages.
use crate::{OperationId, files as m, guest as w};
use anyhow::{Context as _, Result, ensure};

pub const CAPTURE_TTL_SECS: u64 = 60;
pub fn digest(bytes: Vec<u8>) -> Result<[u8; 32]> {
    bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("invalid file digest length"))
}
impl From<&m::Upload> for w::FileUpload {
    fn from(v: &m::Upload) -> Self {
        Self {
            operation_id: v.operation_id.to_string(),
            path: v.path.clone(),
            size: v.size,
            sha256: v.sha256.to_vec(),
            mode: v.mode,
        }
    }
}
impl TryFrom<w::FileUpload> for m::Upload {
    type Error = anyhow::Error;
    fn try_from(v: w::FileUpload) -> Result<Self> {
        let result = Self {
            operation_id: v.operation_id.parse()?,
            path: v.path,
            size: v.size,
            sha256: digest(v.sha256)?,
            mode: v.mode,
        };
        result.validate()?;
        Ok(result)
    }
}
impl From<&m::Receipt> for w::FileReceipt {
    fn from(v: &m::Receipt) -> Self {
        Self {
            version: v.version,
            context: Some((&v.context).into()),
            upload: Some((&v.upload).into()),
            digest: v.digest.to_vec(),
            state: match v.state {
                m::State::Staging => w::FileState::Staging,
                m::State::CommitIntent => w::FileState::CommitIntent,
                m::State::Committed => w::FileState::Committed,
                m::State::Unknown => w::FileState::Unknown,
                m::State::Aborted => w::FileState::Aborted,
            } as i32,
        }
    }
}
impl TryFrom<w::FileReceipt> for m::Receipt {
    type Error = anyhow::Error;
    fn try_from(v: w::FileReceipt) -> Result<Self> {
        let result = Self {
            version: v.version,
            context: v.context.context("missing file context")?.try_into()?,
            upload: v.upload.context("missing upload descriptor")?.try_into()?,
            digest: digest(v.digest)?,
            state: match w::FileState::try_from(v.state)? {
                w::FileState::Staging => m::State::Staging,
                w::FileState::CommitIntent => m::State::CommitIntent,
                w::FileState::Committed => m::State::Committed,
                w::FileState::Unknown => m::State::Unknown,
                w::FileState::Aborted => m::State::Aborted,
                w::FileState::Unspecified => anyhow::bail!("unspecified file state"),
            },
        };
        result.validate()?;
        Ok(result)
    }
}
pub fn operation(upload: &m::Upload) -> Result<w::FileOperation> {
    Ok(w::FileOperation {
        operation_id: upload.operation_id.to_string(),
        digest: upload.digest()?.to_vec(),
    })
}
pub fn validate_operation(v: &w::FileOperation) -> Result<OperationId> {
    ensure!(v.digest.len() == 32, "invalid upload operation digest");
    Ok(v.operation_id.parse()?)
}
pub fn validate_write(v: &w::WriteFile) -> Result<OperationId> {
    let id = validate_operation(v.operation.as_ref().context("missing upload operation")?)?;
    ensure!(
        !v.data.is_empty()
            && v.data.len() <= m::MAX_CHUNK_BYTES
            && v.offset
                .checked_add(v.data.len() as u64)
                .is_some_and(|n| n <= m::MAX_FILE_BYTES),
        "invalid upload chunk"
    );
    Ok(id)
}
pub fn validate_handle(v: &w::FileHandle) -> Result<OperationId> {
    ensure!(v.sha256.len() == 32, "invalid capture digest");
    Ok(v.capture_id.parse()?)
}
pub fn validate_capture(v: &w::FileCapture) -> Result<()> {
    v.capture_id.parse::<OperationId>()?;
    m::validate_path(&v.path)?;
    ensure!(
        v.sha256.len() == 32 && v.size <= m::MAX_FILE_BYTES && v.expires_unix_ms > 0,
        "invalid file capture"
    );
    Ok(())
}
pub fn handle(v: &w::FileCapture) -> Result<w::FileHandle> {
    validate_capture(v)?;
    Ok(w::FileHandle {
        capture_id: v.capture_id.clone(),
        sha256: v.sha256.clone(),
    })
}
pub fn validate_read(v: &w::ReadFile) -> Result<OperationId> {
    let id = validate_handle(v.handle.as_ref().context("missing capture handle")?)?;
    ensure!(
        (1..=m::MAX_CHUNK_BYTES as u32).contains(&v.limit) && v.offset <= m::MAX_FILE_BYTES,
        "invalid file read range"
    );
    Ok(id)
}
impl std::fmt::Debug for w::WriteFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WriteFile")
            .field("offset", &self.offset)
            .field("length", &self.data.len())
            .finish_non_exhaustive()
    }
}
impl std::fmt::Debug for w::FileChunk {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FileChunk")
            .field("offset", &self.offset)
            .field("length", &self.data.len())
            .finish_non_exhaustive()
    }
}
