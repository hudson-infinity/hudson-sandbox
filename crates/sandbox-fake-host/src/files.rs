//! Bounded simulated staging. No host or guest filesystem is touched.
use super::*;
use sandbox_protocol::{
    files as m,
    guest_model::Context,
    supervisor::{FileObservation, FileRequest, FileWriteRequest},
    supervisor_files::{FileRecord, MAX_FILES, MAX_HOST_FILES},
};
use sha2::{Digest, Sha256};
#[derive(Debug)]
pub(super) enum FileAction {
    Begin,
    Inspect,
    Commit,
    Abort,
    Write(u64, Vec<u8>),
}
impl FakeHost {
    pub async fn lose_next_file_reply(&self) {
        self.state.lock().await.lose_next_file_reply = true;
    }
    pub async fn total_file_commits(&self) -> u64 {
        self.state.lock().await.file_commits
    }
    pub(super) async fn file_write_inner(
        &self,
        r: FileWriteRequest,
    ) -> Result<FileObservation, Status> {
        if r.data.is_empty()
            || r.data.len() > m::MAX_CHUNK_BYTES
            || r.offset
                .checked_add(r.data.len() as u64)
                .is_none_or(|n| n > m::MAX_FILE_BYTES)
        {
            return Err(Status::invalid_argument("invalid file chunk"));
        }
        self.file_inner(
            r.request
                .ok_or_else(|| Status::invalid_argument("file request required"))?,
            FileAction::Write(r.offset, r.data),
        )
        .await
    }
    pub(super) async fn file_inner(
        &self,
        r: FileRequest,
        action: FileAction,
    ) -> Result<FileObservation, Status> {
        let mut state = self.state.lock().await;
        Self::expire(&mut state);
        let now = unix_ms()?;
        let owner = self.ownership(r.ownership, now)?;
        let upload: m::Upload = r
            .upload
            .ok_or_else(|| Status::invalid_argument("upload required"))?
            .try_into()
            .map_err(|_| Status::invalid_argument("invalid upload"))?;
        if upload.operation_id.to_string() != owner.operation_id {
            return Err(Status::invalid_argument("file operation mismatch"));
        }
        if let FileAction::Write(offset, data) = &action
            && offset
                .checked_add(data.len() as u64)
                .is_none_or(|n| n > upload.size)
        {
            return Err(Status::invalid_argument("chunk exceeds file size"));
        }
        let digest = upload
            .digest()
            .map_err(|_| Status::invalid_argument("file digest"))?;
        let ready = state
            .allocations
            .get(&owner.allocation_id)
            .is_some_and(|a| a.state == AllocationState::Ready);
        if let Some(fence) = state.fences.get(&owner.allocation_id)
            && (fence.owner.project_id != owner.project_id
                || fence.owner.sandbox_id != owner.sandbox_id
                || fence.owner.generation != owner.generation)
        {
            return Err(Status::failed_precondition("allocation identity mismatch"));
        }
        let previous = state
            .fences
            .get(&owner.allocation_id)
            .and_then(|f| f.files.get(&owner.operation_id))
            .cloned();
        let new = previous.is_none();
        if let Some(file) = &previous {
            file.validate_identity(upload.operation_id, &upload)
                .map_err(|_| Status::already_exists("file descriptor changed"))?;
        }
        if new {
            if state.fences.values().map(|f| f.files.len()).sum::<usize>() >= MAX_HOST_FILES {
                return Err(Status::resource_exhausted("fake file journal full"));
            }
            if let Some(f) = state.fences.get(&owner.allocation_id) {
                if f.revisions.contains_key(&owner.operation_id)
                    || f.commands.contains_key(&owner.operation_id)
                {
                    return Err(Status::already_exists("operation has another purpose"));
                }
                if f.files.len() >= MAX_FILES
                    || f.files
                        .values()
                        .map(|f| f.size)
                        .sum::<u64>()
                        .saturating_add(
                            if matches!(action, FileAction::Begin) && ready && !f.stopped {
                                upload.size
                            } else {
                                0
                            },
                        )
                        > m::MAX_RESERVED_BYTES
                {
                    return Err(Status::resource_exhausted("fake file capacity full"));
                }
            }
        }
        Self::fence(&mut state, &owner)?;
        let retained_bytes: usize = state
            .fences
            .values()
            .flat_map(|f| f.file_data.values())
            .map(Vec::len)
            .sum();
        let f = state
            .fences
            .get_mut(&owner.allocation_id)
            .ok_or_else(|| Status::internal("missing fence"))?;
        let mut file = previous.unwrap_or_else(|| FileRecord::fenced(digest));
        if new && matches!(action, FileAction::Begin) && ready && !f.stopped {
            file = FileRecord::pending(
                &upload,
                Context {
                    allocation_id: upload_id(&owner)?,
                    generation: owner.generation,
                    boot_id: "simulated-guest-boot".into(),
                },
            )
            .map_err(|_| Status::internal("file intent"))?;
            file.state = 1;
            f.file_data.insert(owner.operation_id.clone(), Vec::new());
        }
        let mut stored = None;
        let mut committed = false;
        let mut uncertain = false;
        if !file.finished() && ready && !f.stopped {
            match action {
                FileAction::Write(offset, data)
                    if file.state == 1 && !file.commit_requested && !file.abort_requested =>
                {
                    let bytes = f
                        .file_data
                        .get_mut(&owner.operation_id)
                        .ok_or_else(|| Status::unavailable("missing fake staging"))?;
                    if offset > bytes.len() as u64 {
                        return Err(Status::failed_precondition("noncontiguous chunk"));
                    }
                    let start = offset as usize;
                    let overlap = (bytes.len() - start).min(data.len());
                    if bytes[start..start + overlap] != data[..overlap] {
                        return Err(Status::already_exists("changed chunk"));
                    }
                    if retained_bytes + data.len() - overlap > m::MAX_RESERVED_BYTES as usize {
                        return Err(Status::resource_exhausted("fake staging memory full"));
                    }
                    bytes.extend_from_slice(&data[overlap..]);
                    stored = Some(bytes.len() as u64);
                }
                FileAction::Commit
                    if file.state == 1 && !file.commit_requested && !file.abort_requested =>
                {
                    file.commit_requested = true;
                    let bytes = f
                        .file_data
                        .get(&owner.operation_id)
                        .ok_or_else(|| Status::unavailable("missing fake staging"))?;
                    if bytes.len() as u64 == upload.size
                        && <[u8; 32]>::from(Sha256::digest(bytes)) == upload.sha256
                    {
                        file.state = 3;
                        committed = true;
                        f.file_data.remove(&owner.operation_id);
                    } else {
                        uncertain = true;
                    }
                }
                FileAction::Abort if !file.abort_requested => {
                    file.abort_requested = true;
                    file.state = 5;
                    f.file_data.remove(&owner.operation_id);
                }
                _ => {}
            }
        } else if !file.finished() {
            uncertain = true;
        }
        let mut observation = file.observation(owner.clone(), true, now, stored);
        if uncertain {
            observation.state = 0;
        }
        f.files.insert(owner.operation_id, file);
        if committed {
            state.file_commits += 1;
        }
        if std::mem::take(&mut state.lose_next_file_reply) {
            return Err(Status::unavailable("injected lost file reply"));
        }
        Ok(observation)
    }
}
fn upload_id(owner: &Ownership) -> Result<AllocationId, Status> {
    owner
        .allocation_id
        .parse()
        .map_err(|_| Status::invalid_argument("allocation"))
}
