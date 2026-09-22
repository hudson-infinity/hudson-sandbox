//! Project-scoped HTTPS client. Models and requests are generated from OpenAPI.
//! No automatic retries: retain each mutation key and payload across uncertainty.
mod config;
mod files;
pub mod models;
pub mod requests;
mod stream;
mod transport;
pub use files::{Download, read_upload};
use reqwest::header::HeaderValue;
use std::{fmt, path::Path, time::Duration};
pub use stream::{Event, EventStream};
pub use transport::RangeChunk;
use url::Url;

/// Errors deliberately omit URLs, credentials, request bodies and backend titles.
#[derive(thiserror::Error)]
pub enum Error {
    #[error("invalid private client configuration or credential file")]
    Config,
    #[error("invalid or oversized request")]
    Request,
    #[error("transport failed; mutation outcome may be unknown; retain the same key and payload")]
    Transport,
    #[error("HTTP request failed with status {status}")]
    Http { status: u16, code: Option<String> },
    #[error("invalid, oversized or inconsistent server response")]
    Protocol,
    #[error("wait deadline reached; the operation continues; poll the same operation")]
    WaitTimeout,
    #[error("local file operation failed; destination must not already exist")]
    File,
    #[error("download integrity verification failed")]
    Integrity,
}
impl fmt::Debug for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}
impl Error {
    /// Known static API code safe for diagnostic output; unknown additive values remain
    /// available in the HTTP variant but are never reflected into diagnostics.
    pub fn problem_code(&self) -> Option<&'static str> {
        match self {
            Self::Http {
                code: Some(code), ..
            } => Some(requests::known_problem_code(code).unwrap_or("unrecognized_problem_code")),
            _ => None,
        }
    }
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Config => "configuration",
            Self::Request => "request",
            Self::Transport => "transport",
            Self::Http { .. } => "http",
            Self::Protocol => "protocol",
            Self::WaitTimeout => "wait_timeout",
            Self::File => "file",
            Self::Integrity => "integrity",
        }
    }
}

#[derive(Clone)]
pub struct Client {
    http: reqwest::Client,
    origin: Url,
    token: HeaderValue,
    request_timeout: Duration,
}
impl fmt::Debug for Client {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Client").finish_non_exhaustive()
    }
}
impl Client {
    /// Config and credentials are trusted local files, never workload arguments.
    pub fn from_config(path: &Path) -> Result<Self, Error> {
        config::load(path)
    }

    /// Polls only this operation. Timeout and disconnect never cancel or resubmit it.
    pub async fn wait(
        &self,
        operation_id: &str,
        deadline: Duration,
    ) -> Result<models::OperationBody, Error> {
        if deadline.is_zero() || deadline > Duration::from_secs(86400) {
            return Err(Error::Request);
        }
        tokio::time::timeout(deadline, async {
            loop {
                let op = self
                    .get_operation(requests::GetOperation { operation_id })
                    .await?;
                if op.operation_id != operation_id {
                    return Err(Error::Protocol);
                }
                match op.status.as_str() {
                    "queued" | "running" => tokio::time::sleep(Duration::from_millis(500)).await,
                    "succeeded" | "failed" | "cancelled" | "unknown" => return Ok(op),
                    _ => return Err(Error::Protocol),
                }
            }
        })
        .await
        .map_err(|_| Error::WaitTimeout)?
    }
}

/// Generate once per logical mutation. Save before sending; reuse after uncertainty.
pub fn new_idempotency_key() -> String {
    uuid::Uuid::new_v4().to_string()
}

pub fn valid_key(value: &str) -> bool {
    (16..=128).contains(&value.len())
        && value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c))
}
