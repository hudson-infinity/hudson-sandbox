use crate::{Client, Error};
use reqwest::header::HeaderValue;
use serde::Deserialize;
use std::{
    path::{Path, PathBuf},
    time::Duration,
};
use url::Url;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    version: u32,
    endpoint: String,
    credential_file: PathBuf,
    ca_file: Option<PathBuf>,
    #[serde(default = "default_timeout")]
    request_timeout_seconds: u64,
}
fn default_timeout() -> u64 {
    30
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Credential {
    version: u32,
    project_id: String,
    name: String,
    token: String,
    created_at: i64,
    expires_at: i64,
}

// fstat the opened descriptor: no symlink, FIFO, device, unbounded or public file.
#[cfg(unix)]
fn private_bytes(path: &Path) -> Result<Vec<u8>, Error> {
    use std::{
        io::Read,
        os::unix::fs::{MetadataExt, OpenOptionsExt},
    };
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags((rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK).bits() as i32)
        .open(path)
        .map_err(|_| Error::Config)?;
    let meta = file.metadata().map_err(|_| Error::Config)?;
    if !meta.is_file()
        || meta.uid() != rustix::process::geteuid().as_raw()
        || meta.mode() & 0o077 != 0
        || meta.len() > 65536
    {
        return Err(Error::Config);
    }
    let mut bytes = Vec::new();
    file.take(65537)
        .read_to_end(&mut bytes)
        .map_err(|_| Error::Config)?;
    if bytes.len() > 65536 {
        return Err(Error::Config);
    }
    Ok(bytes)
}
#[cfg(not(unix))]
fn private_bytes(_: &Path) -> Result<Vec<u8>, Error> {
    Err(Error::Config)
}

pub(super) fn load(path: &Path) -> Result<Client, Error> {
    let config: Config =
        serde_json::from_slice(&private_bytes(path)?).map_err(|_| Error::Config)?;
    let origin = Url::parse(&config.endpoint).map_err(|_| Error::Config)?;
    if config.version != 1
        || !(1..=120).contains(&config.request_timeout_seconds)
        || origin.scheme() != "https"
        || origin.host().is_none()
        || !origin.username().is_empty()
        || origin.password().is_some()
        || origin.path() != "/"
        || origin.query().is_some()
        || origin.fragment().is_some()
    {
        return Err(Error::Config);
    }
    let parent = path.parent().ok_or(Error::Config)?;
    let credential: Credential =
        serde_json::from_slice(&private_bytes(&parent.join(config.credential_file))?)
            .map_err(|_| Error::Config)?;
    if credential.version != 1
        || !credential.project_id.starts_with("prj_")
        || credential.name.len() > 4096
        || credential.created_at < 0
        || credential.expires_at <= credential.created_at
        || credential.token.is_empty()
        || credential.token.len() > 4096
        || credential.token.bytes().any(|b| !b.is_ascii_graphic())
    {
        return Err(Error::Config);
    }
    let mut token = HeaderValue::from_str(&format!("Bearer {}", credential.token))
        .map_err(|_| Error::Config)?;
    token.set_sensitive(true);
    let _ = tokio_rustls::rustls::crypto::ring::default_provider().install_default();
    let mut builder = reqwest::Client::builder()
        .https_only(true)
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .connect_timeout(Duration::from_secs(5))
        .read_timeout(Duration::from_secs(20));
    if let Some(ca_file) = config.ca_file {
        let certs = reqwest::Certificate::from_pem_bundle(&private_bytes(&parent.join(ca_file))?)
            .map_err(|_| Error::Config)?;
        if certs.is_empty() {
            return Err(Error::Config);
        }
        builder = builder.tls_certs_only(certs);
    }
    Ok(Client {
        http: builder.build().map_err(|_| Error::Config)?,
        origin,
        token,
        request_timeout: Duration::from_secs(config.request_timeout_seconds),
    })
}
