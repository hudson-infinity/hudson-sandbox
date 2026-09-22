use crate::{
    Client, Error,
    models::{AdmittedResponse, FileCaptureRequest, FileCaptureResponse},
    requests::*,
};
use sha2::{Digest, Sha256};
use std::{
    io::{Read, Write},
    path::Path,
};

#[derive(Debug, serde::Serialize)]
pub struct Download {
    pub size: u64,
    pub sha256: String,
    pub simulated: bool,
    pub guest_reported: bool,
    /// False means release was not acknowledged; the bounded capture still expires.
    pub release_confirmed: bool,
}
impl Client {
    pub async fn upload(
        &self,
        sandbox_id: &str,
        path: &str,
        bytes: &[u8],
        mode: &str,
        key: &str,
    ) -> Result<AdmittedResponse, Error> {
        if bytes.len() > 8388608 || !matches!(mode, "0644" | "0755") {
            return Err(Error::Request);
        }
        self.upload_file(UploadFile {
            sandbox_id,
            path,
            idempotency_key: key,
            x_file_size: bytes.len() as u64,
            x_file_sha256: &hex::encode(Sha256::digest(bytes)),
            x_file_mode: Some(mode),
            body: bytes,
        })
        .await
    }
    /// Capture once, verify every chunk and the full digest, then atomically publish
    /// a private local file without replacing any existing destination.
    pub async fn download(
        &self,
        sandbox_id: &str,
        path: &str,
        destination: &Path,
    ) -> Result<Download, Error> {
        let parent = destination
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let mut file = tempfile::NamedTempFile::new_in(parent).map_err(|_| Error::File)?;
        let capture = self
            .capture_file(CaptureFile {
                sandbox_id,
                body: &FileCaptureRequest { path: path.into() },
            })
            .await?;
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(60),
            self.download_capture(sandbox_id, &capture, &mut file),
        )
        .await
        .map_err(|_| Error::Transport)
        .and_then(|r| r);
        let released = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            self.release_file_capture(ReleaseFileCapture {
                sandbox_id,
                x_file_capture: &capture.capture,
            }),
        )
        .await
        .is_ok_and(|r| r.is_ok());
        result?;
        file.as_file().sync_all().map_err(|_| Error::File)?;
        file.persist_noclobber(destination)
            .map_err(|_| Error::File)?;
        Ok(Download {
            size: capture.size,
            sha256: capture.sha256,
            simulated: capture.simulated,
            guest_reported: capture.guest_reported,
            release_confirmed: released,
        })
    }
    async fn download_capture(
        &self,
        sandbox_id: &str,
        capture: &FileCaptureResponse,
        file: &mut tempfile::NamedTempFile,
    ) -> Result<(), Error> {
        if capture.size > 8388608
            || capture.chunk_size != 32768
            || !capture.guest_reported
            || !crate::transport::valid_sha(&capture.sha256)
        {
            return Err(Error::Protocol);
        }
        let mut offset = 0;
        let mut digest = Sha256::new();
        loop {
            let chunk = self
                .read_captured_file(ReadCapturedFile {
                    sandbox_id,
                    x_file_capture: &capture.capture,
                    offset: Some(offset),
                    limit: Some(32768),
                })
                .await?;
            if chunk.size != capture.size
                || chunk.sha256.as_ref() != Some(&capture.sha256)
                || chunk.simulated != capture.simulated
            {
                return Err(Error::Integrity);
            }
            file.write_all(&chunk.bytes).map_err(|_| Error::File)?;
            digest.update(&chunk.bytes);
            offset = chunk.next_offset;
            if chunk.eof {
                break;
            }
        }
        if offset != capture.size || hex::encode(digest.finalize()) != capture.sha256 {
            return Err(Error::Integrity);
        }
        Ok(())
    }
}
/// Read a regular local upload without following a symlink or blocking on a FIFO.
#[cfg(unix)]
pub fn read_upload(path: &Path) -> Result<Vec<u8>, Error> {
    use std::os::unix::fs::OpenOptionsExt;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags((rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK).bits() as i32)
        .open(path)
        .map_err(|_| Error::File)?;
    let meta = file.metadata().map_err(|_| Error::File)?;
    if !meta.is_file() || meta.len() > 8388608 {
        return Err(Error::File);
    }
    let mut bytes = Vec::new();
    file.take(8388609)
        .read_to_end(&mut bytes)
        .map_err(|_| Error::File)?;
    if bytes.len() > 8388608 {
        return Err(Error::File);
    }
    Ok(bytes)
}
#[cfg(not(unix))]
pub fn read_upload(_: &Path) -> Result<Vec<u8>, Error> {
    Err(Error::File)
}
