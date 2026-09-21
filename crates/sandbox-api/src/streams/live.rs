//! Trusted read-only host adapter. Endpoints and credentials are operator inputs.
use crate::problem::Problem;
use sandbox_protocol::{
    HostId,
    supervisor::{LiveOutputObservation, LiveOutputRequest},
};
use std::{fmt, future::Future, pin::Pin};

pub trait LiveReader: fmt::Debug + Send + Sync {
    fn read<'a>(
        &'a self,
        host: HostId,
        request: LiveOutputRequest,
    ) -> Pin<Box<dyn Future<Output = Result<LiveOutputObservation, Problem>> + Send + 'a>>;
}
pub struct LiveClient {
    host: HostId,
    endpoint: String,
    ca: Vec<u8>,
    cert: Vec<u8>,
    key: Vec<u8>,
    allow_simulated: bool,
}
impl fmt::Debug for LiveClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LiveClient")
            .field("host", &self.host)
            .finish_non_exhaustive()
    }
}
impl LiveClient {
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
            .map_err(|_| anyhow::anyhow!("invalid live host endpoint"))?;
        anyhow::ensure!(
            uri.scheme_str() == Some("https")
                && uri.host().is_some()
                && uri.authority().is_some_and(|a| !a.as_str().contains('@'))
                && uri.path() == "/"
                && uri.query().is_none(),
            "invalid live host endpoint"
        );
        anyhow::ensure!(
            !ca.is_empty()
                && !cert.is_empty()
                && !key.is_empty()
                && ca.len() <= 65536
                && cert.len() <= 65536
                && key.len() <= 65536,
            "invalid live host TLS files"
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
}
impl LiveReader for LiveClient {
    fn read<'a>(
        &'a self,
        host: HostId,
        request: LiveOutputRequest,
    ) -> Pin<Box<dyn Future<Output = Result<LiveOutputObservation, Problem>> + Send + 'a>> {
        Box::pin(async move {
            if host != self.host {
                return Err(Problem::Unavailable);
            }
            let mut client = sandbox_supervisor::transport::connect_output_reader(
                &self.endpoint,
                host,
                &self.ca,
                &self.cert,
                &self.key,
            )
            .await
            .map_err(|_| Problem::Unavailable)?;
            let reply = client
                .read(request)
                .await
                .map_err(|e| match e.code() {
                    tonic::Code::NotFound | tonic::Code::FailedPrecondition => {
                        Problem::OutputMissing
                    }
                    tonic::Code::OutOfRange => Problem::OutputRange,
                    _ => Problem::Unavailable,
                })?
                .into_inner();
            if reply.simulated && !self.allow_simulated || reply.host_id != self.host.to_string() {
                return Err(Problem::Unavailable);
            }
            Ok(reply)
        })
    }
}
