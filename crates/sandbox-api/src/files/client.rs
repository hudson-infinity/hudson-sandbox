//! Read-only transport. All endpoints and TLS material are selected by the operator.
use crate::problem::Problem;
use sandbox_protocol::{
    HostId,
    supervisor::{
        FileAccessObservation, FileCaptureRequest, FileDownloadRequest, FileReleaseRequest,
    },
};
use std::{fmt, future::Future, pin::Pin};
pub type Reply<'a> =
    Pin<Box<dyn Future<Output = Result<FileAccessObservation, Problem>> + Send + 'a>>;
pub trait FileReader: fmt::Debug + Send + Sync {
    fn capture(&self, host: HostId, request: FileCaptureRequest) -> Reply<'_>;
    fn read(&self, host: HostId, request: FileDownloadRequest) -> Reply<'_>;
    fn release(&self, host: HostId, request: FileReleaseRequest) -> Reply<'_>;
}
pub struct FileClient {
    host: HostId,
    endpoint: String,
    ca: Vec<u8>,
    cert: Vec<u8>,
    key: Vec<u8>,
    allow_simulated: bool,
}
impl fmt::Debug for FileClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FileClient")
            .field("host", &self.host)
            .finish_non_exhaustive()
    }
}
impl FileClient {
    pub fn new(
        host: HostId,
        endpoint: String,
        ca: Vec<u8>,
        cert: Vec<u8>,
        key: Vec<u8>,
        allow_simulated: bool,
    ) -> anyhow::Result<Self> {
        let uri: http::Uri = endpoint
            .parse()
            .map_err(|_| anyhow::anyhow!("invalid file host endpoint"))?;
        anyhow::ensure!(
            uri.scheme_str() == Some("https")
                && uri.host().is_some()
                && uri.authority().is_some_and(|a| !a.as_str().contains('@'))
                && uri.path() == "/"
                && uri.query().is_none(),
            "invalid file host endpoint"
        );
        anyhow::ensure!(
            [&ca, &cert, &key]
                .iter()
                .all(|v| !v.is_empty() && v.len() <= 65536),
            "invalid file host TLS material"
        );
        Ok(Self {
            host,
            endpoint,
            ca,
            cert,
            key,
            allow_simulated,
        })
    }
    async fn connect(
        &self,
        host: HostId,
    ) -> Result<
        sandbox_protocol::supervisor::file_downloads_client::FileDownloadsClient<
            tonic::transport::Channel,
        >,
        Problem,
    > {
        if host != self.host {
            return Err(Problem::Unavailable);
        }
        sandbox_supervisor::transport::connect_file_reader(
            &self.endpoint,
            host,
            &self.ca,
            &self.cert,
            &self.key,
        )
        .await
        .map_err(|_| Problem::Unavailable)
    }
    fn response(
        &self,
        response: Result<tonic::Response<FileAccessObservation>, tonic::Status>,
    ) -> Result<FileAccessObservation, Problem> {
        let reply = response
            .map_err(|e| match e.code() {
                tonic::Code::NotFound | tonic::Code::FailedPrecondition => Problem::FileMissing,
                _ => Problem::Unavailable,
            })?
            .into_inner();
        if reply.simulated && !self.allow_simulated {
            return Err(Problem::Unavailable);
        }
        Ok(reply)
    }
}
impl FileReader for FileClient {
    fn capture(&self, host: HostId, r: FileCaptureRequest) -> Reply<'_> {
        Box::pin(async move { self.response(self.connect(host).await?.capture(r).await) })
    }
    fn read(&self, host: HostId, r: FileDownloadRequest) -> Reply<'_> {
        Box::pin(async move { self.response(self.connect(host).await?.read(r).await) })
    }
    fn release(&self, host: HostId, r: FileReleaseRequest) -> Reply<'_> {
        Box::pin(async move { self.response(self.connect(host).await?.release(r).await) })
    }
}

/// Optional operator wiring. API callers never supply an endpoint or TLS material.
#[derive(Debug, clap::Args)]
pub struct FileReaderArgs {
    #[arg(long,requires_all=["file_endpoint","file_ca_cert","file_client_cert","file_client_key"])]
    file_host_id: Option<HostId>,
    #[arg(long, requires = "file_host_id")]
    file_endpoint: Option<String>,
    #[arg(long, requires = "file_host_id")]
    file_ca_cert: Option<std::path::PathBuf>,
    #[arg(long, requires = "file_host_id")]
    file_client_cert: Option<std::path::PathBuf>,
    #[arg(long, requires = "file_host_id")]
    file_client_key: Option<std::path::PathBuf>,
    #[arg(long, requires = "file_host_id")]
    allow_simulated_files: bool,
}
impl FileReaderArgs {
    pub fn build(self) -> anyhow::Result<Option<std::sync::Arc<dyn FileReader>>> {
        let Some(host) = self.file_host_id else {
            return Ok(None);
        };
        let read = |path: Option<std::path::PathBuf>| -> anyhow::Result<Vec<u8>> {
            use std::io::Read;
            let path = path.ok_or_else(|| anyhow::anyhow!("file reader TLS path required"))?;
            let mut bytes = Vec::new();
            std::fs::File::open(path)?
                .take(65537)
                .read_to_end(&mut bytes)?;
            anyhow::ensure!(bytes.len() <= 65536, "file reader TLS material too large");
            Ok(bytes)
        };
        Ok(Some(std::sync::Arc::new(FileClient::new(
            host,
            self.file_endpoint
                .ok_or_else(|| anyhow::anyhow!("file endpoint required"))?,
            read(self.file_ca_cert)?,
            read(self.file_client_cert)?,
            read(self.file_client_key)?,
            self.allow_simulated_files,
        )?)))
    }
}
