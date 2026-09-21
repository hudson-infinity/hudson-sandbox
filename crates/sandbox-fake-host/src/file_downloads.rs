//! Simulated captured downloads of files published by the fake upload service.
use super::*;
use sandbox_protocol::{
    Id,
    file_downloads::{self as model, ReadScope},
    file_wire, files as fm, guest,
    guest_model::Context,
    supervisor::{
        FileAccessObservation, FileCaptureRequest, FileDownloadRequest, FileReleaseRequest,
        file_access_observation::Result as FileResult, file_downloads_server::FileDownloads,
    },
};
use sandbox_supervisor::file_downloads as downloads;
use sha2::{Digest, Sha256};
impl FakeHost {
    pub async fn lose_next_download_reply(&self) {
        self.state.lock().await.lose_next_download_reply = true;
    }
    pub async fn total_file_captures(&self) -> u64 {
        self.state.lock().await.file_captures
    }
    fn download_scope(&self, state: &State, scope: &ReadScope) -> Result<Context, Status> {
        if scope.host_id != self.config.host || scope.host_epoch != self.config.epoch {
            return Err(Status::failed_precondition("wrong file host epoch"));
        }
        let fence = state
            .fences
            .get(&scope.allocation_id.to_string())
            .ok_or_else(downloads::missing)?;
        if !scope.matches_owner(&fence.owner) {
            return Err(Status::failed_precondition("file allocation mismatch"));
        }
        if fence.stopped
            || state
                .allocations
                .get(&scope.allocation_id.to_string())
                .is_none_or(|a| a.state != AllocationState::Ready || a.expires <= Instant::now())
        {
            return Err(downloads::unavailable("fake allocation stopped"));
        }
        Ok(Context {
            allocation_id: scope.allocation_id,
            generation: scope.generation,
            boot_id: "simulated-guest-boot".into(),
        })
    }
    async fn capture_download(
        &self,
        r: FileCaptureRequest,
    ) -> Result<FileAccessObservation, Status> {
        let mut state = self.state.lock().await;
        let scope = model::capture_request(&r, unix_ms()?).map_err(downloads::invalid)?;
        let context = self.download_scope(&state, &scope)?;
        prune(&mut state);
        let bytes = state
            .published_files
            .get(&(scope.allocation_id.to_string(), r.path.clone()))
            .ok_or_else(downloads::missing)?;
        if state
            .captured_files
            .values()
            .map(|(_, b)| b.len())
            .sum::<usize>()
            + bytes.len()
            > fm::MAX_RESERVED_BYTES as usize
        {
            return Err(Status::resource_exhausted("fake captured bytes full"));
        }
        let bytes = bytes.clone();
        let wall = unix_ms()?;
        let id = state.downloads.reserve(
            scope.clone(),
            context,
            r.path.clone(),
            wall,
            Instant::now().into_std(),
        )?;
        let capture = guest::FileCapture {
            capture_id: OperationId::generate().to_string(),
            path: r.path,
            size: bytes.len() as u64,
            sha256: Sha256::digest(&bytes).to_vec(),
            expires_unix_ms: wall + 60_000,
        };
        let handle = state
            .downloads
            .finish(id, capture, Instant::now().into_std())?;
        state
            .captured_files
            .insert(id, (scope.allocation_id.to_string(), bytes));
        state.file_captures += 1;
        observe(&mut state, r.scope_json, FileResult::Captured(handle))
    }
    async fn read_download(&self, r: FileDownloadRequest) -> Result<FileAccessObservation, Status> {
        let mut state = self.state.lock().await;
        let scope = model::read_request(&r, unix_ms()?).map_err(downloads::invalid)?;
        let context = self.download_scope(&state, &scope)?;
        prune(&mut state);
        let handle = r.handle.as_ref().ok_or_else(downloads::missing)?;
        if state
            .downloads
            .validate(&scope, handle, Instant::now().into_std())?
        {
            return Err(downloads::missing());
        }
        if handle.context.as_ref() != Some(&(&context).into()) {
            return Err(Status::failed_precondition("file boot changed"));
        }
        let id = handle.id.parse().map_err(downloads::invalid)?;
        let (_, bytes) = state
            .captured_files
            .get(&id)
            .ok_or_else(downloads::missing)?;
        let capture = handle.capture.as_ref().ok_or_else(downloads::missing)?;
        let next = (r.offset + r.limit as u64).min(bytes.len() as u64);
        let chunk = guest::FileChunk {
            handle: Some(file_wire::handle(capture).map_err(downloads::invalid)?),
            offset: r.offset,
            data: bytes[r.offset as usize..next as usize].to_vec(),
            next_offset: next,
            size: bytes.len() as u64,
            at_end: next == bytes.len() as u64,
        };
        observe(&mut state, r.scope_json, FileResult::Chunk(chunk))
    }
    async fn release_download(
        &self,
        r: FileReleaseRequest,
    ) -> Result<FileAccessObservation, Status> {
        let mut state = self.state.lock().await;
        let scope = model::release_request(&r, unix_ms()?).map_err(downloads::invalid)?;
        self.download_scope(&state, &scope)?;
        prune(&mut state);
        let handle = r.handle.as_ref().ok_or_else(downloads::missing)?;
        state
            .downloads
            .released(&scope, handle, Instant::now().into_std())?;
        state
            .captured_files
            .remove(&handle.id.parse().map_err(downloads::invalid)?);
        observe(
            &mut state,
            r.scope_json,
            FileResult::Released(handle.clone()),
        )
    }
}
fn prune(state: &mut State) {
    state.downloads.prune(Instant::now().into_std());
    state
        .captured_files
        .retain(|id, _| state.downloads.contains(id));
}
fn observe(
    state: &mut State,
    scope_json: Vec<u8>,
    result: FileResult,
) -> Result<FileAccessObservation, Status> {
    if std::mem::take(&mut state.lose_next_download_reply) {
        return Err(Status::unavailable("injected lost download reply"));
    }
    Ok(FileAccessObservation {
        scope_json,
        simulated: true,
        observed_unix_ms: unix_ms()?,
        result: Some(result),
    })
}
#[tonic::async_trait]
impl FileDownloads for FakeHost {
    async fn capture(
        &self,
        r: Request<FileCaptureRequest>,
    ) -> Result<Response<FileAccessObservation>, Status> {
        let host = self.clone();
        self.file_readers
            .run(async move { host.capture_download(r.into_inner()).await })
            .await
            .map(Response::new)
    }
    async fn read(
        &self,
        r: Request<FileDownloadRequest>,
    ) -> Result<Response<FileAccessObservation>, Status> {
        let host = self.clone();
        self.file_readers
            .run(async move { host.read_download(r.into_inner()).await })
            .await
            .map(Response::new)
    }
    async fn release(
        &self,
        r: Request<FileReleaseRequest>,
    ) -> Result<Response<FileAccessObservation>, Status> {
        let host = self.clone();
        self.file_readers
            .run(async move { host.release_download(r.into_inner()).await })
            .await
            .map(Response::new)
    }
}
