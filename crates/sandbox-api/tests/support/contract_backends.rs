//! Synthetic byte sources for wire-contract checks, never runtime/isolation evidence.
use sandbox_api::{
    files::client::{FileReader, Reply},
    outputs::OutputReader,
};
use sandbox_artifacts::{
    Error, OutputChunk,
    sources::{SourceBackend, SourceBytes},
};
use sandbox_protocol::{
    HostId, Id, OperationId,
    file_downloads::ReadScope,
    file_sources::{SourceOwner, SourcePlan, SourceRef},
    guest,
    output::{OutputName, OutputOwner, OutputRef},
    supervisor::{
        FileAccessObservation, FileCaptureRequest, FileDownloadHandle, FileDownloadRequest,
        FileReleaseRequest, file_access_observation::Result as ResultBody,
    },
};
use sha2::{Digest, Sha256};
use std::{future::Future, pin::Pin};
#[derive(Debug)]
pub(super) struct Backend;
fn now() -> i64 {
    (time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000) as i64
}
fn reply(scope_json: Vec<u8>, result: ResultBody) -> FileAccessObservation {
    FileAccessObservation {
        scope_json,
        result: Some(result),
        simulated: true,
        observed_unix_ms: now(),
    }
}
const FILE: &[u8] = b"hello\0\xff";
impl FileReader for Backend {
    fn capture(&self, _: HostId, r: FileCaptureRequest) -> Reply<'_> {
        Box::pin(async move {
            let scope: ReadScope = serde_json::from_slice(&r.scope_json).unwrap();
            Ok(reply(
                r.scope_json,
                ResultBody::Captured(FileDownloadHandle {
                    id: OperationId::generate().to_string(),
                    context: Some(guest::Context {
                        allocation_id: scope.allocation_id.to_string(),
                        generation: scope.generation,
                        boot_id: "contract-boot".into(),
                    }),
                    capture: Some(guest::FileCapture {
                        capture_id: OperationId::generate().to_string(),
                        path: r.path,
                        size: FILE.len() as u64,
                        sha256: Sha256::digest(FILE).to_vec(),
                        expires_unix_ms: now() + 60000,
                    }),
                    expires_unix_ms: now() + 60000,
                }),
            ))
        })
    }
    fn read(&self, _: HostId, r: FileDownloadRequest) -> Reply<'_> {
        Box::pin(async move {
            let c = r.handle.unwrap().capture.unwrap();
            let next = (r.offset + r.limit as u64).min(c.size);
            Ok(reply(
                r.scope_json,
                ResultBody::Chunk(guest::FileChunk {
                    handle: Some(guest::FileHandle {
                        capture_id: c.capture_id,
                        sha256: c.sha256,
                    }),
                    offset: r.offset,
                    data: FILE[r.offset as usize..next as usize].to_vec(),
                    next_offset: next,
                    size: c.size,
                    at_end: next == c.size,
                }),
            ))
        })
    }
    fn release(&self, _: HostId, r: FileReleaseRequest) -> Reply<'_> {
        Box::pin(async move { Ok(reply(r.scope_json, ResultBody::Released(r.handle.unwrap()))) })
    }
}
impl OutputReader for Backend {
    fn read<'a>(
        &'a self,
        r: &'a OutputRef,
        _: &'a OutputOwner,
        _: i64,
        offset: u64,
        limit: usize,
    ) -> Pin<Box<dyn Future<Output = Result<OutputChunk, Error>> + Send + 'a>> {
        Box::pin(async move {
            let bytes = match r.plan.name {
                OutputName::Stdout => b"a\0b\xff".as_slice(),
                OutputName::Stderr => b"err".as_slice(),
            };
            let next = bytes.len().min(offset as usize + limit);
            Ok(OutputChunk {
                bytes: bytes[offset as usize..next].to_vec(),
                next_offset: next as u64,
                eof: next == bytes.len(),
                truncated: r.plan.truncated,
            })
        })
    }
}
impl SourceBackend for Backend {
    fn upload<'a>(
        &'a self,
        p: &'a SourcePlan,
        _: &'a SourceOwner,
        _: i64,
        b: &'a [u8],
    ) -> Pin<Box<dyn Future<Output = Result<SourceRef, Error>> + Send + 'a>> {
        Box::pin(async move {
            assert_eq!(p.upload.size, b.len() as u64);
            assert_eq!(p.upload.sha256.as_slice(), Sha256::digest(b).as_slice());
            Ok(SourceRef {
                plan: p.clone(),
                etag: "contract-etag".into(),
                object_version: None,
            })
        })
    }
    fn reconcile<'a>(
        &'a self,
        _: &'a SourcePlan,
        _: &'a SourceOwner,
        _: i64,
    ) -> Pin<Box<dyn Future<Output = Result<SourceRef, Error>> + Send + 'a>> {
        Box::pin(async { Err(Error::Unavailable) })
    }
    fn read<'a>(
        &'a self,
        _: &'a SourceRef,
        _: &'a SourceOwner,
        _: i64,
    ) -> Pin<Box<dyn Future<Output = Result<SourceBytes, Error>> + Send + 'a>> {
        Box::pin(async { Err(Error::Unavailable) })
    }
}
