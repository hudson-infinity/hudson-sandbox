//! Compact host-owned file evidence. File paths and bytes are never retained here.
use crate::{
    OperationId, files as m,
    guest_model::Context,
    supervisor::{FileObservation, Ownership},
};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
pub const MAX_FILES: usize = 16;
pub const MAX_HOST_FILES: usize = 1024;
pub const MAX_RECORD_BYTES: usize = 2048;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileRecord {
    pub digest: [u8; 32],
    pub context: Option<Context>,
    pub size: u64,
    pub not_started: bool,
    pub commit_requested: bool,
    pub abort_requested: bool,
    /// 0 means no observation; 1..=5 are the guest FileState values. Numeric encoding
    /// keeps subsequent observations the same size; retained file updates cannot grow it.
    pub state: u8,
}
impl FileRecord {
    pub fn fenced(digest: [u8; 32]) -> Self {
        Self {
            digest,
            context: None,
            size: 0,
            not_started: true,
            commit_requested: false,
            abort_requested: false,
            state: 0,
        }
    }
    pub fn pending(upload: &m::Upload, context: Context) -> Result<Self> {
        let record = Self {
            digest: upload.digest()?,
            context: Some(context),
            size: upload.size,
            not_started: false,
            commit_requested: false,
            abort_requested: false,
            state: 0,
        };
        record.validate()?;
        Ok(record)
    }
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.size <= m::MAX_FILE_BYTES && self.state <= 5,
            "invalid retained file bounds"
        );
        if self.not_started {
            ensure!(
                self.context.is_none()
                    && self.size == 0
                    && self.state == 0
                    && !self.commit_requested
                    && !self.abort_requested,
                "invalid file absence fence"
            );
        } else {
            let context = self
                .context
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("missing file context"))?;
            ensure!(
                context.generation > 0
                    && !context.boot_id.is_empty()
                    && context.boot_id.len() <= 64,
                "invalid file context"
            );
            ensure!(
                !matches!(self.state, 2 | 3) || self.commit_requested,
                "file committed without host intent"
            );
            ensure!(
                self.state != 5 || self.abort_requested,
                "file aborted without host intent"
            );
        }
        ensure!(
            serde_json::to_vec(self)?.len() <= MAX_RECORD_BYTES,
            "file record exceeds bound"
        );
        Ok(())
    }
    pub fn finished(&self) -> bool {
        self.not_started || matches!(self.state, 3..=5)
    }
    pub fn observe(&mut self, upload: &m::Upload, receipt: &m::Receipt) -> Result<()> {
        receipt.validate()?;
        ensure!(
            !self.not_started
                && self.context.as_ref() == Some(&receipt.context)
                && receipt.upload == *upload
                && self.digest == receipt.digest
                && self.size == upload.size,
            "file receipt differs from original intent"
        );
        let next = match receipt.state {
            m::State::Staging => 1,
            m::State::CommitIntent => 2,
            m::State::Committed => 3,
            m::State::Unknown => 4,
            m::State::Aborted => 5,
        };
        ensure!(
            !self.finished() || self.state == next,
            "terminal file receipt changed"
        );
        ensure!(
            !matches!(next, 2 | 3) || self.commit_requested,
            "unsolicited file commit"
        );
        ensure!(next != 5 || self.abort_requested, "unsolicited file abort");
        self.state = next;
        self.validate()
    }
    pub fn observation(
        &self,
        owner: Ownership,
        simulated: bool,
        now: i64,
        stored: Option<u64>,
    ) -> FileObservation {
        FileObservation {
            ownership: Some(owner),
            simulated,
            observed_unix_ms: now,
            upload_digest: self.digest.to_vec(),
            context: self.context.as_ref().map(Into::into),
            state: self.state.into(),
            not_started: self.not_started,
            stored,
        }
    }
    pub fn validate_identity(&self, id: OperationId, upload: &m::Upload) -> Result<()> {
        ensure!(
            upload.operation_id == id && upload.digest()? == self.digest,
            "file descriptor changed"
        );
        Ok(())
    }
}

impl std::fmt::Debug for crate::supervisor::FileWriteRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FileWriteRequest")
            .field("offset", &self.offset)
            .field("length", &self.data.len())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::{AllocationId, Id};
    fn fixture() -> (m::Upload, FileRecord) {
        let upload = m::Upload {
            operation_id: OperationId::generate(),
            path: "private-customer-path".into(),
            size: m::MAX_FILE_BYTES,
            sha256: [255; 32],
            mode: 0o755,
        };
        let record = FileRecord::pending(
            &upload,
            Context {
                allocation_id: AllocationId::generate(),
                generation: i64::MAX,
                boot_id: "\u{1}".repeat(64),
            },
        )
        .unwrap();
        (upload, record)
    }
    #[test]
    fn file_results_fit_reserved_metadata_and_do_not_retain_paths() {
        let (upload, initial) = fixture();
        let length = serde_json::to_vec(&initial).unwrap().len();
        assert!(length <= MAX_RECORD_BYTES);
        assert!(
            !serde_json::to_string(&initial)
                .unwrap()
                .contains(&upload.path)
        );
        for state in [
            m::State::Staging,
            m::State::CommitIntent,
            m::State::Committed,
            m::State::Unknown,
            m::State::Aborted,
        ] {
            let mut record = initial.clone();
            record.commit_requested = true;
            record.abort_requested = true;
            let receipt = m::Receipt {
                version: 1,
                context: record.context.clone().unwrap(),
                upload: upload.clone(),
                digest: record.digest,
                state,
            };
            record.observe(&upload, &receipt).unwrap();
            assert!(serde_json::to_vec(&record).unwrap().len() <= length);
        }
    }
    #[test]
    fn unsolicited_or_changed_file_receipts_never_become_committed() {
        let (upload, mut record) = fixture();
        let mut receipt = m::Receipt {
            version: 1,
            context: record.context.clone().unwrap(),
            upload: upload.clone(),
            digest: record.digest,
            state: m::State::Committed,
        };
        assert!(record.observe(&upload, &receipt).is_err());
        record.commit_requested = true;
        receipt.context.generation = 1;
        assert!(record.observe(&upload, &receipt).is_err());
        receipt.context = record.context.clone().unwrap();
        record.observe(&upload, &receipt).unwrap();
        receipt.state = m::State::Staging;
        assert!(record.observe(&upload, &receipt).is_err());
        let mut bad = record;
        bad.state = 6;
        assert!(bad.validate().is_err());
    }
    #[test]
    fn absence_fences_cannot_gain_guest_evidence_or_dispatch_flags() {
        let (upload, _) = fixture();
        let mut record = FileRecord::fenced(upload.digest().unwrap());
        record.validate().unwrap();
        record.commit_requested = true;
        assert!(record.validate().is_err());
        let body = crate::supervisor::FileWriteRequest {
            data: b"sensitive-file-content".to_vec(),
            ..Default::default()
        };
        assert!(!format!("{body:?}").contains("sensitive-file-content"));
    }
}
