use super::*;
use crate::file_downloads as downloads;
use sandbox_protocol::{
    file_downloads::{self as model, ReadScope},
    guest_model::Context,
    supervisor::{
        FileAccessObservation, FileCaptureRequest, FileDownloadRequest, FileReleaseRequest,
        file_access_observation::Result as FileResult, file_downloads_server::FileDownloads,
    },
};

impl Host {
    fn download_record(&self, scope: &ReadScope) -> Result<Manifest, Status> {
        if scope.host_id != self.inner.config.host || scope.host_epoch != self.inner.config.epoch {
            return Err(Status::failed_precondition(
                "file read requires current host epoch",
            ));
        }
        let j = self.journal()?;
        let r = j
            .records
            .get(&scope.allocation_id.to_string())
            .ok_or_else(downloads::missing)?;
        if !scope.matches_owner(&r.owner) {
            return Err(Status::failed_precondition("file read allocation mismatch"));
        }
        if r.stopped || r.released {
            return Err(downloads::unavailable("allocation stopped"));
        }
        r.manifest.clone().ok_or_else(downloads::missing)
    }
    async fn download_prepare(
        &self,
        scope: ReadScope,
    ) -> Result<(crate::guest::GuestClient, tokio::sync::OwnedSemaphorePermit), Status> {
        self.work(move |h| {
            let manifest = h.download_record(&scope)?;
            let permit = h
                .journal()?
                .records
                .get(&scope.allocation_id.to_string())
                .ok_or_else(downloads::missing)?
                .file_io
                .clone()
                .try_acquire_owned()
                .map_err(|_| Status::resource_exhausted("allocation file worker busy"))?;
            let client = manifest.guest_client().map_err(downloads::unavailable)?;
            Ok((client, permit))
        })
        .await
    }
    async fn download_client(&self, scope: ReadScope) -> Result<crate::guest::GuestClient, Status> {
        self.work(move |h| {
            h.download_record(&scope)?
                .guest_client()
                .map_err(downloads::unavailable)
        })
        .await
    }
    async fn check_download_current(
        &self,
        scope: ReadScope,
        context: &Context,
    ) -> Result<(), Status> {
        let current = self.download_client(scope).await?;
        if current.context() != context {
            return Err(Status::failed_precondition("file read boot changed"));
        }
        Ok(())
    }
    async fn capture_download(
        &self,
        r: FileCaptureRequest,
    ) -> Result<FileAccessObservation, Status> {
        let scope = model::capture_request(&r, guardian::wall_ms()).map_err(downloads::invalid)?;
        let (client, _file_io) = self.download_prepare(scope.clone()).await?;
        model::capture_request(&r, guardian::wall_ms()).map_err(downloads::invalid)?;
        let id = lock(&self.inner.downloads)?.reserve(
            scope.clone(),
            client.context().clone(),
            r.path.clone(),
            guardian::wall_ms(),
            Instant::now(),
        )?;
        // A failed or lost response retains its ticket until expiry; no automatic recapture.
        let capture = client
            .capture_file(&r.path)
            .await
            .map_err(downloads::unavailable)?;
        self.check_download_current(scope, client.context()).await?;
        model::capture_request(&r, guardian::wall_ms()).map_err(downloads::invalid)?;
        let handle = lock(&self.inner.downloads)?.finish(id, capture, Instant::now())?;
        Ok(access_observation(
            r.scope_json,
            FileResult::Captured(handle),
        ))
    }
    async fn read_download(&self, r: FileDownloadRequest) -> Result<FileAccessObservation, Status> {
        let scope = model::read_request(&r, guardian::wall_ms()).map_err(downloads::invalid)?;
        let handle = r.handle.as_ref().ok_or_else(downloads::missing)?;
        if lock(&self.inner.downloads)?.validate(&scope, handle, Instant::now())? {
            return Err(downloads::missing());
        }
        let context: Context = handle
            .context
            .clone()
            .ok_or_else(downloads::missing)?
            .try_into()
            .map_err(downloads::invalid)?;
        let (client, _file_io) = self.download_prepare(scope.clone()).await?;
        if client.context() != &context {
            return Err(Status::failed_precondition("file read boot changed"));
        }
        model::read_request(&r, guardian::wall_ms()).map_err(downloads::invalid)?;
        let chunk = client
            .read_file(
                handle.capture.as_ref().ok_or_else(downloads::missing)?,
                r.offset,
                r.limit,
            )
            .await
            .map_err(downloads::unavailable)?;
        self.check_download_current(scope.clone(), &context).await?;
        model::read_request(&r, guardian::wall_ms()).map_err(downloads::invalid)?;
        if lock(&self.inner.downloads)?.validate(&scope, handle, Instant::now())? {
            return Err(downloads::missing());
        }
        Ok(access_observation(r.scope_json, FileResult::Chunk(chunk)))
    }
    async fn release_download(
        &self,
        r: FileReleaseRequest,
    ) -> Result<FileAccessObservation, Status> {
        let scope = model::release_request(&r, guardian::wall_ms()).map_err(downloads::invalid)?;
        let handle = r.handle.as_ref().ok_or_else(downloads::missing)?;
        let released = lock(&self.inner.downloads)?.validate(&scope, handle, Instant::now())?;
        let context: Context = handle
            .context
            .clone()
            .ok_or_else(downloads::missing)?
            .try_into()
            .map_err(downloads::invalid)?;
        let (client, _file_io) = self.download_prepare(scope.clone()).await?;
        if client.context() != &context {
            return Err(Status::failed_precondition("file read boot changed"));
        }
        model::release_request(&r, guardian::wall_ms()).map_err(downloads::invalid)?;
        if !released {
            client
                .release_file(handle.capture.as_ref().ok_or_else(downloads::missing)?)
                .await
                .map_err(downloads::unavailable)?;
        }
        self.check_download_current(scope.clone(), &context).await?;
        model::release_request(&r, guardian::wall_ms()).map_err(downloads::invalid)?;
        lock(&self.inner.downloads)?.released(&scope, handle, Instant::now())?;
        Ok(access_observation(
            r.scope_json,
            FileResult::Released(handle.clone()),
        ))
    }
}
fn access_observation(scope_json: Vec<u8>, result: FileResult) -> FileAccessObservation {
    FileAccessObservation {
        scope_json,
        simulated: false,
        observed_unix_ms: guardian::wall_ms(),
        result: Some(result),
    }
}
#[tonic::async_trait]
impl FileDownloads for Host {
    async fn capture(
        &self,
        r: Request<FileCaptureRequest>,
    ) -> Result<Response<FileAccessObservation>, Status> {
        let host = self.clone();
        self.inner
            .file_readers
            .run(async move { host.capture_download(r.into_inner()).await })
            .await
            .map(Response::new)
    }
    async fn read(
        &self,
        r: Request<FileDownloadRequest>,
    ) -> Result<Response<FileAccessObservation>, Status> {
        let host = self.clone();
        self.inner
            .file_readers
            .run(async move { host.read_download(r.into_inner()).await })
            .await
            .map(Response::new)
    }
    async fn release(
        &self,
        r: Request<FileReleaseRequest>,
    ) -> Result<Response<FileAccessObservation>, Status> {
        let host = self.clone();
        self.inner
            .file_readers
            .run(async move { host.release_download(r.into_inner()).await })
            .await
            .map(Response::new)
    }
}
