//! Authenticated transport adapter for one configured guest workspace.
//! One blocking worker owns file I/O; timing out a caller never drops its permit early.
use crate::files::{Download, Transfers};
use anyhow::{Context as _, Result, ensure};
use sandbox_protocol::{
    Id, OperationId, file_wire as wire, files as m, guest as w, guest_model::Context,
};
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::sync::Semaphore;

const WORK_TIMEOUT: Duration = Duration::from_secs(5);
struct Capture {
    descriptor: w::FileCapture,
    bytes: Download,
    until: Instant,
}
struct State {
    transfers: Transfers,
    captures: BTreeMap<OperationId, Capture>,
}
struct Inner {
    context: Context,
    state: Mutex<State>,
    capacity: Arc<Semaphore>,
    closed: AtomicBool,
    reaper: Mutex<Option<tokio::task::JoinHandle<()>>>,
}
#[derive(Clone)]
pub struct FileService(Arc<Inner>);
impl std::fmt::Debug for FileService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FileService")
            .field("context", &self.0.context)
            .finish_non_exhaustive()
    }
}
impl FileService {
    pub async fn open(path: PathBuf, context: Context) -> Result<Self> {
        let saved = context.clone();
        let transfers =
            tokio::task::spawn_blocking(move || Transfers::open(&path, saved)).await??;
        let inner = Arc::new(Inner {
            context,
            state: Mutex::new(State {
                transfers,
                captures: BTreeMap::new(),
            }),
            capacity: Arc::new(Semaphore::new(1)),
            closed: AtomicBool::new(false),
            reaper: Mutex::new(None),
        });
        let weak = Arc::downgrade(&inner);
        // Expiry runs even if the client disappears and never makes another request.
        let reaper = tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(1)).await;
                let Some(inner) = weak.upgrade() else {
                    break;
                };
                if inner.closed.load(Ordering::Acquire) {
                    break;
                }
                // Only bounded in-memory buffers are dropped here. Never queue behind I/O
                // or compete for the file worker permit during an otherwise idle request.
                if let Ok(mut state) = inner.state.try_lock() {
                    state.prune();
                }
            }
        });
        *inner
            .reaper
            .lock()
            .map_err(|_| anyhow::anyhow!("file reaper poisoned"))? = Some(reaper);
        Ok(Self(inner))
    }
    pub fn context(&self) -> &Context {
        &self.0.context
    }
    pub async fn call(&self, action: w::request::Action) -> Result<w::response::Result> {
        self.run(move |state| state.dispatch(action)).await
    }
    async fn run<T: Send + 'static>(
        &self,
        work: impl FnOnce(&mut State) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        ensure!(
            !self.0.closed.load(Ordering::Acquire),
            "file service closed"
        );
        let permit = self
            .0
            .capacity
            .clone()
            .try_acquire_owned()
            .context("file worker busy")?;
        let inner = self.0.clone();
        let task = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let mut state = inner
                .state
                .lock()
                .map_err(|_| anyhow::anyhow!("file worker poisoned"))?;
            ensure!(!inner.closed.load(Ordering::Acquire), "file service closed");
            state.prune();
            work(&mut state)
        });
        tokio::time::timeout(WORK_TIMEOUT, task)
            .await
            .context("file outcome uncertain: worker deadline")??
    }
    /// Fence admission before waiting for an admitted worker; an unconfirmed stop is an error.
    pub async fn shutdown(&self) -> Result<()> {
        self.0.closed.store(true, Ordering::Release);
        let reaper = self
            .0
            .reaper
            .lock()
            .map_err(|_| anyhow::anyhow!("file reaper poisoned"))?
            .take();
        if let Some(reaper) = reaper {
            reaper.abort();
            let _ = reaper.await;
        }
        let permit = tokio::time::timeout(WORK_TIMEOUT, self.0.capacity.clone().acquire_owned())
            .await
            .context("file worker shutdown unconfirmed")??;
        let mut state = self
            .0
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("file worker poisoned"))?;
        state.captures.clear();
        drop(permit);
        Ok(())
    }
}
impl State {
    fn prune(&mut self) {
        let now = Instant::now();
        self.captures.retain(|_, v| v.until > now);
    }
    fn upload(&self, operation: &w::FileOperation) -> Result<m::Receipt> {
        let id = wire::validate_operation(operation)?;
        let receipt = self.transfers.inspect(id)?.context("upload not found")?;
        ensure!(
            receipt.digest.as_slice() == operation.digest,
            "upload digest conflicts"
        );
        Ok(receipt)
    }
    fn capture(&self, handle: &w::FileHandle) -> Result<&Capture> {
        let id = wire::validate_handle(handle)?;
        let capture = self
            .captures
            .get(&id)
            .context("capture expired or not found")?;
        ensure!(
            capture.descriptor.sha256 == handle.sha256,
            "capture digest conflicts"
        );
        Ok(capture)
    }
    fn dispatch(&mut self, action: w::request::Action) -> Result<w::response::Result> {
        use w::{request::Action as A, response::Result as R};
        Ok(match action {
            A::BeginUpload(v) => R::FileReceipt((&self.transfers.begin(v.try_into()?)?).into()),
            A::WriteFile(v) => {
                wire::validate_write(&v)?;
                let operation = v.operation.context("missing upload operation")?;
                let receipt = self.upload(&operation)?;
                let stored =
                    self.transfers
                        .write_chunk(receipt.upload.operation_id, v.offset, &v.data)?;
                R::FileProgress(w::FileProgress {
                    operation: Some(operation),
                    stored,
                })
            }
            A::InspectUpload(v) => R::FileReceipt((&self.upload(&v)?).into()),
            A::CommitUpload(v) => {
                let id = self.upload(&v)?.upload.operation_id;
                R::FileReceipt((&self.transfers.commit(id)?).into())
            }
            A::AbortUpload(v) => {
                let id = self.upload(&v)?.upload.operation_id;
                R::FileReceipt((&self.transfers.abort(id)?).into())
            }
            A::CaptureFile(v) => {
                m::validate_path(&v.path)?;
                ensure!(self.captures.len() < 8, "capture capacity exhausted");
                let bytes = self.transfers.capture(&v.path)?;
                let id = OperationId::generate();
                let descriptor = w::FileCapture {
                    capture_id: id.to_string(),
                    path: v.path,
                    size: bytes.size(),
                    sha256: bytes.sha256.to_vec(),
                    expires_unix_ms: crate::runner::now_ms() + wire::CAPTURE_TTL_SECS as i64 * 1000,
                };
                self.captures.insert(
                    id,
                    Capture {
                        descriptor: descriptor.clone(),
                        bytes,
                        until: Instant::now() + Duration::from_secs(wire::CAPTURE_TTL_SECS),
                    },
                );
                R::FileCapture(descriptor)
            }
            A::ReadFile(v) => {
                wire::validate_read(&v)?;
                let handle = v.handle.context("missing capture handle")?;
                let capture = self.capture(&handle)?;
                let data = capture.bytes.chunk(v.offset, v.limit as usize)?.to_vec();
                let next_offset = v.offset + data.len() as u64;
                R::FileChunk(w::FileChunk {
                    handle: Some(handle),
                    offset: v.offset,
                    data,
                    next_offset,
                    size: capture.descriptor.size,
                    at_end: next_offset == capture.descriptor.size,
                })
            }
            A::ReleaseFile(v) => {
                let id = wire::validate_handle(&v)?;
                if self.captures.contains_key(&id) {
                    self.capture(&v)?;
                    self.captures.remove(&id);
                }
                R::FileReleased(v)
            }
            _ => anyhow::bail!("unsupported file action"),
        })
    }
}

#[cfg(test)]
#[path = "file_service_tests.rs"]
mod tests;
