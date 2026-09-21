//! Bounded ephemeral capture tickets. No paths are reopened to recreate a missing ticket.
use sandbox_protocol::{
    Id, OperationId,
    file_downloads::{self as model, ReadScope},
    file_wire, guest,
    guest_model::Context,
    supervisor::FileDownloadHandle,
};
use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};
use tonic::Status;
pub const MAX_DOWNLOADS: usize = 64;
pub const MAX_ALLOCATION_DOWNLOADS: usize = 8;
pub const TTL: Duration = Duration::from_secs(60);
pub const CALL_TIMEOUT: Duration = Duration::from_secs(4);

struct Ticket {
    scope: ReadScope,
    context: Context,
    path: String,
    expires: Instant,
    expires_unix_ms: i64,
    handle: Option<FileDownloadHandle>,
    released: bool,
}
#[derive(Default)]
pub struct Registry {
    tickets: BTreeMap<OperationId, Ticket>,
}
impl std::fmt::Debug for Registry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DownloadRegistry")
            .field("tickets", &self.tickets.len())
            .finish()
    }
}
impl Registry {
    pub fn prune(&mut self, now: Instant) {
        self.tickets.retain(|_, t| t.expires > now);
    }
    pub fn contains(&self, id: &OperationId) -> bool {
        self.tickets.contains_key(id)
    }
    pub fn reserve(
        &mut self,
        scope: ReadScope,
        context: Context,
        path: String,
        wall: i64,
        now: Instant,
    ) -> Result<OperationId, Status> {
        scope.validate().map_err(invalid)?;
        sandbox_protocol::files::validate_path(&path).map_err(invalid)?;
        if context.allocation_id != scope.allocation_id
            || context.generation != scope.generation
            || context.boot_id.is_empty()
            || context.boot_id.len() > 64
        {
            return Err(invalid("capture context"));
        }
        self.prune(now);
        if self.tickets.len() >= MAX_DOWNLOADS
            || self
                .tickets
                .values()
                .filter(|t| t.scope.allocation_id == scope.allocation_id)
                .count()
                >= MAX_ALLOCATION_DOWNLOADS
        {
            return Err(Status::resource_exhausted("file capture tickets full"));
        }
        let id = OperationId::generate();
        self.tickets.insert(
            id,
            Ticket {
                scope,
                context,
                path,
                expires: now + TTL,
                expires_unix_ms: wall.saturating_add(TTL.as_millis() as i64),
                handle: None,
                released: false,
            },
        );
        Ok(id)
    }
    pub fn finish(
        &mut self,
        id: OperationId,
        capture: guest::FileCapture,
        now: Instant,
    ) -> Result<FileDownloadHandle, Status> {
        self.prune(now);
        let t = self.tickets.get_mut(&id).ok_or_else(missing)?;
        file_wire::validate_capture(&capture).map_err(unavailable)?;
        if capture.path != t.path || t.handle.is_some() {
            return Err(unavailable("capture identity changed"));
        }
        let h = FileDownloadHandle {
            id: id.to_string(),
            context: Some((&t.context).into()),
            capture: Some(capture),
            expires_unix_ms: t.expires_unix_ms,
        };
        t.handle = Some(h.clone());
        Ok(h)
    }
    /// True means a matching release was acknowledged. No expiry or missing entry authorizes capture.
    pub fn validate(
        &mut self,
        scope: &ReadScope,
        handle: &FileDownloadHandle,
        now: Instant,
    ) -> Result<bool, Status> {
        model::handle(handle, scope).map_err(invalid)?;
        self.prune(now);
        let id = handle.id.parse().map_err(invalid)?;
        let t = self.tickets.get(&id).ok_or_else(missing)?;
        if &t.scope != scope || t.handle.as_ref() != Some(handle) {
            return Err(Status::failed_precondition(
                "file capture ownership or descriptor changed",
            ));
        }
        Ok(t.released)
    }
    pub fn released(
        &mut self,
        scope: &ReadScope,
        handle: &FileDownloadHandle,
        now: Instant,
    ) -> Result<(), Status> {
        self.validate(scope, handle, now)?;
        let id = handle.id.parse().map_err(invalid)?;
        self.tickets.get_mut(&id).ok_or_else(missing)?.released = true;
        Ok(())
    }
}
pub fn missing() -> Status {
    Status::not_found("file capture unavailable or expired")
}
pub fn invalid(_: impl std::fmt::Display) -> Status {
    Status::invalid_argument("invalid file read request")
}
pub fn unavailable(_: impl std::fmt::Display) -> Status {
    Status::unavailable("file read unavailable")
}

/// Four detached calls, each with a bounded caller budget. Guest file workers retain
/// their own permits if filesystem work outlives the host's request deadline.
#[derive(Debug, Clone)]
pub struct Workers {
    permits: std::sync::Arc<tokio::sync::Semaphore>,
}
impl Default for Workers {
    fn default() -> Self {
        Self {
            permits: std::sync::Arc::new(tokio::sync::Semaphore::new(4)),
        }
    }
}
impl Workers {
    pub async fn run<T: Send + 'static>(
        &self,
        task: impl std::future::Future<Output = Result<T, Status>> + Send + 'static,
    ) -> Result<T, Status> {
        let permit = self
            .permits
            .clone()
            .try_acquire_owned()
            .map_err(|_| Status::resource_exhausted("file readers busy"))?;
        tokio::spawn(async move {
            let _permit = permit;
            tokio::time::timeout(CALL_TIMEOUT, task)
                .await
                .map_err(unavailable)?
        })
        .await
        .map_err(unavailable)?
    }
}
