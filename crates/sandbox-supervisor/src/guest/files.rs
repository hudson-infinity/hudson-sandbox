//! File methods share GuestClient's exact mTLS peer, boot and response correlation checks.
use super::GuestClient;
use anyhow::{Context as _, Result, ensure};
use sandbox_protocol::{file_wire as wire, files as m, guest as w};
impl GuestClient {
    async fn file_receipt(
        &self,
        upload: &m::Upload,
        action: w::request::Action,
    ) -> Result<m::Receipt> {
        upload.validate()?;
        let w::response::Result::FileReceipt(value) = self.call(action).await?.1 else {
            anyhow::bail!("unexpected file receipt response");
        };
        let receipt: m::Receipt = value.try_into()?;
        ensure!(
            receipt.context == self.context
                && receipt.upload == *upload
                && receipt.digest == upload.digest()?,
            "file receipt ownership or payload mismatch"
        );
        Ok(receipt)
    }
    pub async fn begin_upload(&self, upload: &m::Upload) -> Result<m::Receipt> {
        upload.validate()?;
        self.file_receipt(upload, w::request::Action::BeginUpload(upload.into()))
            .await
    }
    pub async fn inspect_upload(&self, upload: &m::Upload) -> Result<m::Receipt> {
        self.file_receipt(
            upload,
            w::request::Action::InspectUpload(wire::operation(upload)?),
        )
        .await
    }
    pub async fn commit_upload(&self, upload: &m::Upload) -> Result<m::Receipt> {
        self.file_receipt(
            upload,
            w::request::Action::CommitUpload(wire::operation(upload)?),
        )
        .await
    }
    pub async fn abort_upload(&self, upload: &m::Upload) -> Result<m::Receipt> {
        self.file_receipt(
            upload,
            w::request::Action::AbortUpload(wire::operation(upload)?),
        )
        .await
    }
    pub async fn write_file(&self, upload: &m::Upload, offset: u64, data: &[u8]) -> Result<u64> {
        let operation = wire::operation(upload)?;
        ensure!(
            !data.is_empty()
                && data.len() <= m::MAX_CHUNK_BYTES
                && offset
                    .checked_add(data.len() as u64)
                    .is_some_and(|n| n <= upload.size),
            "invalid upload chunk"
        );
        let request = w::WriteFile {
            operation: Some(operation.clone()),
            offset,
            data: data.to_vec(),
        };
        let w::response::Result::FileProgress(progress) =
            self.call(w::request::Action::WriteFile(request)).await?.1
        else {
            anyhow::bail!("unexpected file progress response");
        };
        ensure!(
            progress.operation.as_ref() == Some(&operation)
                && progress.stored >= offset + data.len() as u64
                && progress.stored <= upload.size,
            "file progress mismatch"
        );
        Ok(progress.stored)
    }
    /// A capture is a fresh read, never an automatic retry. A lost response can consume one
    /// bounded guest handle until expiry. Range reads never recreate missing captures.
    pub async fn capture_file(&self, path: &str) -> Result<w::FileCapture> {
        m::validate_path(path)?;
        let w::response::Result::FileCapture(capture) = self
            .call(w::request::Action::CaptureFile(w::CaptureFile {
                path: path.into(),
            }))
            .await?
            .1
        else {
            anyhow::bail!("unexpected file capture response");
        };
        wire::validate_capture(&capture)?;
        ensure!(capture.path == path, "capture path mismatch");
        Ok(capture)
    }
    /// The caller verifies the whole captured SHA-256 when assembling a file. A range response
    /// proves neither whole-file integrity nor guest honesty merely by echoing that digest.
    pub async fn read_file(
        &self,
        capture: &w::FileCapture,
        offset: u64,
        limit: u32,
    ) -> Result<w::FileChunk> {
        let handle = wire::handle(capture)?;
        ensure!(
            offset <= capture.size && (1..=m::MAX_CHUNK_BYTES as u32).contains(&limit),
            "invalid capture range"
        );
        let request = w::ReadFile {
            handle: Some(handle.clone()),
            offset,
            limit,
        };
        let w::response::Result::FileChunk(chunk) =
            self.call(w::request::Action::ReadFile(request)).await?.1
        else {
            anyhow::bail!("unexpected file chunk response");
        };
        let length = (capture.size - offset).min(limit as u64);
        let next = offset.checked_add(length).context("file range overflow")?;
        ensure!(
            chunk.handle.as_ref() == Some(&handle)
                && chunk.offset == offset
                && chunk.size == capture.size
                && chunk.data.len() as u64 == length
                && chunk.next_offset == next
                && chunk.at_end == (next == capture.size),
            "file chunk identity, cursor or size mismatch"
        );
        Ok(chunk)
    }
    pub async fn release_file(&self, capture: &w::FileCapture) -> Result<()> {
        let handle = wire::handle(capture)?;
        let w::response::Result::FileReleased(released) = self
            .call(w::request::Action::ReleaseFile(handle.clone()))
            .await?
            .1
        else {
            anyhow::bail!("unexpected file release response");
        };
        ensure!(released == handle, "file release identity mismatch");
        Ok(())
    }
}
