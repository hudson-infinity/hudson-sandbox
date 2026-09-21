//! File effects remain under the allocation gate and original host/guest ownership.
use super::*;
use sandbox_protocol::{
    files as m,
    supervisor::{FileObservation, FileRequest, FileWriteRequest},
    supervisor_files::{FileRecord, MAX_FILES, MAX_HOST_FILES},
};
#[derive(Debug)]
pub(super) enum FileAction {
    Begin,
    Inspect,
    Commit,
    Abort,
    Write(u64, Vec<u8>),
}
impl Host {
    fn save_file(&self, owner: &Ownership, file: FileRecord) -> Result<(), Status> {
        file.validate().map_err(uncertain)?;
        let mut j = self.journal()?;
        let record = j
            .records
            .get_mut(&owner.allocation_id)
            .ok_or_else(|| uncertain("missing allocation"))?;
        if let Some(old) = record.files.get(&owner.operation_id)
            && serde_json::to_vec(&file).map_err(uncertain)?.len()
                > serde_json::to_vec(old).map_err(uncertain)?.len()
        {
            return Err(uncertain("file update exceeded reserved record size"));
        }
        record.files.insert(owner.operation_id.clone(), file);
        self.save(&mut j)
    }
    fn admit_file(
        &self,
        owner: &Ownership,
        record: &Record,
        file: &FileRecord,
    ) -> Result<(), Status> {
        deadline(owner.claim_expires_unix_ms)?;
        file.validate().map_err(uncertain)?;
        let mut j = self.journal()?;
        if record.revisions.len() >= 64 {
            return Err(Status::resource_exhausted("operation fence capacity full"));
        }
        if record.files.len() >= MAX_FILES
            || j.records.values().map(|r| r.files.len()).sum::<usize>() >= MAX_HOST_FILES
            || record
                .files
                .values()
                .map(|r| r.size)
                .sum::<u64>()
                .saturating_add(file.size)
                > m::MAX_RESERVED_BYTES
        {
            return Err(Status::resource_exhausted("retained file capacity full"));
        }
        if record.revisions.contains_key(&owner.operation_id)
            || record.commands.contains_key(&owner.operation_id)
        {
            return Err(Status::already_exists(
                "operation already has another purpose",
            ));
        }
        // File states use a fixed-width scalar; flags only shrink on transition. Reserve
        // the largest revision up front and leave 256 KiB for existing lifecycle work.
        let mut next = record.clone();
        next.files.insert(owner.operation_id.clone(), file.clone());
        next.revisions.insert(owner.operation_id.clone(), i64::MAX);
        let before = serde_json::to_vec(record).map_err(uncertain)?.len();
        let after = serde_json::to_vec(&next).map_err(uncertain)?.len();
        let total = serde_json::to_vec(&*j).map_err(uncertain)?.len();
        if total
            .saturating_sub(before)
            .saturating_add(after)
            .saturating_add(256 * 1024)
            > journal::MAX_BYTES as usize
        {
            return Err(Status::resource_exhausted(
                "host journal lacks file admission headroom",
            ));
        }
        // Admission and its revision share one durable write under the global journal lock.
        // Two allocations cannot both consume the last host file slot or byte headroom.
        let retained = j
            .records
            .get_mut(&owner.allocation_id)
            .ok_or_else(|| uncertain("missing allocation"))?;
        retained
            .revisions
            .insert(owner.operation_id.clone(), owner.claim_revision);
        retained
            .files
            .insert(owner.operation_id.clone(), file.clone());
        self.save(&mut j)
    }
    pub(super) fn file_write_sync(
        &self,
        request: FileWriteRequest,
    ) -> Result<FileObservation, Status> {
        let r = request
            .request
            .ok_or_else(|| Status::invalid_argument("file request required"))?;
        if request.data.is_empty()
            || request.data.len() > m::MAX_CHUNK_BYTES
            || request
                .offset
                .checked_add(request.data.len() as u64)
                .is_none_or(|n| n > m::MAX_FILE_BYTES)
        {
            return Err(Status::invalid_argument("invalid file chunk"));
        }
        self.file_sync(r, FileAction::Write(request.offset, request.data))
    }
    pub(super) fn file_sync(
        &self,
        request: FileRequest,
        action: FileAction,
    ) -> Result<FileObservation, Status> {
        let owner = self.owner(request.ownership)?;
        let upload: m::Upload = request
            .upload
            .ok_or_else(|| Status::invalid_argument("file descriptor required"))?
            .try_into()
            .map_err(|_| Status::invalid_argument("invalid file descriptor"))?;
        if upload.operation_id.to_string() != owner.operation_id {
            return Err(Status::invalid_argument("file operation mismatch"));
        }
        if let FileAction::Write(offset, data) = &action
            && offset
                .checked_add(data.len() as u64)
                .is_none_or(|n| n > upload.size)
        {
            return Err(Status::invalid_argument("chunk exceeds declared size"));
        }
        let digest = upload.digest().map_err(uncertain)?;
        let gate = self.gate(&owner)?;
        let _guard = lock(&gate)?;
        let record = self
            .journal()?
            .records
            .get(&owner.allocation_id)
            .ok_or_else(|| uncertain("missing record"))?
            .clone();
        let client = if !record.stopped && !record.released {
            record.manifest.as_ref().and_then(|m| m.guest_client().ok())
        } else {
            None
        };
        let mut file = match record.files.get(&owner.operation_id) {
            Some(file) => {
                file.validate_identity(upload.operation_id, &upload)
                    .map_err(|_| Status::already_exists("file descriptor changed"))?;
                file.clone()
            }
            None => {
                if matches!(action, FileAction::Begin)
                    && let Some(client) = &client
                {
                    FileRecord::pending(&upload, client.context().clone()).map_err(uncertain)?
                } else {
                    FileRecord::fenced(digest)
                }
            }
        };
        let new = !record.files.contains_key(&owner.operation_id);
        if new {
            self.admit_file(&owner, &record, &file)?;
        } else {
            self.fence(&owner)?;
        }
        if file.finished() {
            return Ok(file.observation(owner, false, guardian::wall_ms(), None));
        }
        let Some(client) = client else {
            let mut result = file.observation(owner, false, guardian::wall_ms(), None);
            result.state = 0;
            return Ok(result);
        };
        if file.context.as_ref() != Some(client.context()) {
            return Err(uncertain("guest binding changed"));
        }
        deadline(owner.claim_expires_unix_ms)?;
        // Begin/Commit/Abort dispatch intents are recorded once. Retried control calls inspect;
        // chunk retries are allowed only before either terminal intent and remain prefix-checked.
        let response = match action {
            FileAction::Begin if new => guest_call(client.begin_upload(&upload)),
            FileAction::Commit
                if !file.commit_requested && !file.abort_requested && file.state == 1 =>
            {
                file.commit_requested = true;
                self.save_file(&owner, file.clone())?;
                guest_call(client.commit_upload(&upload))
            }
            FileAction::Abort if !file.abort_requested => {
                file.abort_requested = true;
                self.save_file(&owner, file.clone())?;
                guest_call(client.abort_upload(&upload))
            }
            FileAction::Write(offset, data)
                if !file.commit_requested && !file.abort_requested && file.state == 1 =>
            {
                match guest_call(client.write_file(&upload, offset, &data)) {
                    Ok(stored) => {
                        return Ok(file.observation(
                            owner,
                            false,
                            guardian::wall_ms(),
                            Some(stored),
                        ));
                    }
                    Err(_) => Err(anyhow::anyhow!("file chunk outcome unknown")),
                }
            }
            _ => guest_call(client.inspect_upload(&upload)),
        };
        match response {
            Ok(receipt) => {
                file.observe(&upload, &receipt).map_err(uncertain)?;
                self.save_file(&owner, file.clone())?;
                Ok(file.observation(owner, false, guardian::wall_ms(), None))
            }
            Err(_) => {
                let mut result = file.observation(owner, false, guardian::wall_ms(), None);
                result.state = 0;
                Ok(result)
            }
        }
    }
}
fn guest_call<T>(
    future: impl std::future::Future<Output = anyhow::Result<T>>,
) -> anyhow::Result<T> {
    tokio::runtime::Handle::current()
        .block_on(async { tokio::time::timeout(Duration::from_secs(3), future).await? })
}
