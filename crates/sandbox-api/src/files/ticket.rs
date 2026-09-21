use crate::problem::Problem;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use sandbox_protocol::{
    file_downloads::{self as model, ReadScope},
    supervisor::FileDownloadHandle,
};
use sandbox_store::files::FileView;
use serde::{Deserialize, Serialize};
pub(super) const HEADER: &str = "x-file-capture";
pub(super) const MAX_TOKEN: usize = 16384;
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Ticket {
    version: u32,
    pub(super) scope: ReadScope,
    pub(super) handle: FileDownloadHandle,
    pub(super) simulated: bool,
}
impl Ticket {
    pub(super) fn new(view: &FileView, handle: FileDownloadHandle) -> Result<Self, Problem> {
        model::handle(&handle, &view.scope).map_err(|_| Problem::FileCorrupt)?;
        Ok(Self {
            version: 1,
            scope: view.scope.clone(),
            handle,
            simulated: view.simulated,
        })
    }
    pub(super) fn encode(&self) -> Result<String, Problem> {
        let encoded =
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(self).map_err(|_| Problem::Internal)?);
        if encoded.len() > MAX_TOKEN {
            return Err(Problem::FileCorrupt);
        }
        Ok(encoded)
    }
    pub(super) fn parse(
        headers: &http::HeaderMap,
        view: &FileView,
        now: i64,
    ) -> Result<Self, Problem> {
        if headers.get_all(HEADER).iter().count() != 1 {
            return Err(Problem::BadRequest("one X-File-Capture header is required"));
        }
        let raw = headers
            .get(HEADER)
            .and_then(|v| v.to_str().ok())
            .filter(|v| v.len() <= MAX_TOKEN)
            .ok_or(Problem::BadRequest("invalid capture token"))?;
        let ticket: Self = URL_SAFE_NO_PAD
            .decode(raw)
            .ok()
            .and_then(|v| serde_json::from_slice(&v).ok())
            .ok_or(Problem::BadRequest("invalid capture token"))?;
        if ticket.version != 1 {
            return Err(Problem::BadRequest("invalid capture token version"));
        }
        if ticket.scope != view.scope || ticket.simulated != view.simulated {
            return Err(Problem::FileMissing);
        }
        model::handle(&ticket.handle, &view.scope)
            .map_err(|_| Problem::BadRequest("invalid capture token"))?;
        if ticket.handle.expires_unix_ms <= now {
            return Err(Problem::FileMissing);
        }
        Ok(ticket)
    }
}
