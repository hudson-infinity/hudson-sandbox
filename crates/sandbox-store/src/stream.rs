//! Read-only stream resolution from tenant-scoped retained execution evidence.
use crate::{
    Store,
    output::{OutputError, OutputView, execution_evidence},
};
use sandbox_protocol::{
    Id, OperationId, ProjectId, guest_model::State, live_output::LiveOutputScope,
};
use sqlx::Row;
use time::OffsetDateTime;

#[derive(Debug, Clone)]
pub enum StreamSource {
    Pending,
    Missing,
    Expired,
    Live {
        scope: LiveOutputScope,
        simulated: bool,
    },
    Archived(Box<OutputView>),
}
#[derive(Debug, Clone)]
pub struct StreamView {
    pub source: StreamSource,
    pub expires_at: Option<OffsetDateTime>,
}
impl Store {
    /// This query neither claims nor dispatches work. Credential authorization
    /// remains the caller's responsibility before and after any asynchronous read.
    pub async fn stream_for_project(
        &self,
        project: ProjectId,
        operation: OperationId,
    ) -> Result<Option<StreamView>, OutputError> {
        let mut tx = self.pool().begin().await?;
        let row=sqlx::query("SELECT o.*,clock_timestamp() AS read_now FROM operations o JOIN projects p ON p.id=o.project_id WHERE o.id=$1 AND o.project_id=$2 AND o.kind='execute' AND p.status='active'")
            .bind(operation.uuid()).bind(project.uuid()).fetch_optional(&mut *tx).await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let response: Option<OffsetDateTime> = row.try_get("response_expires_at")?;
        let output: Option<OffsetDateTime> = row.try_get("output_expires_at")?;
        let expires_at = match (response, output) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        let now: OffsetDateTime = row.try_get("read_now")?;
        let output_status: String = row.try_get("output_status")?;
        let view = |source| Some(StreamView { source, expires_at });
        if output_status == "expired" || expires_at.is_some_and(|t| t <= now) {
            return Ok(view(StreamSource::Expired));
        }
        if output_status == "published" {
            // Reuse the complete immutable-reference verifier; do not weaken the
            // publication path to accommodate live/nonterminal guest receipts.
            tx.commit().await?;
            return Ok(self
                .output_for_project(project, operation)
                .await?
                .map(|v| StreamView {
                    expires_at: v.expires_at,
                    source: if v.status == "expired" {
                        StreamSource::Expired
                    } else {
                        StreamSource::Archived(Box::new(v))
                    },
                }));
        }
        let history: serde_json::Value = row.try_get("attempt_receipts")?;
        let count: i32 = row.try_get("attempt_count")?;
        if count == 0 {
            return Ok(view(StreamSource::Pending));
        }
        if count != 1 {
            return Err(OutputError::Corrupt);
        }
        let entries = history.as_array().ok_or(OutputError::Corrupt)?;
        if entries.len() == 1 {
            return Ok(view(StreamSource::Pending));
        }
        if entries.len() != 2 {
            return Err(OutputError::Corrupt);
        }
        if entries[1]["phase"] == "command_fenced_before_start" {
            return Ok(view(StreamSource::Missing));
        }
        let e = execution_evidence(&mut tx, &row).await?;
        let status: String = row.try_get("status")?;
        let phase: Option<String> = row.try_get("phase")?;
        let completed: Option<OffsetDateTime> = row.try_get("completed_at")?;
        let valid = match (status.as_str(), phase.as_deref(), e.receipt.state) {
            ("running", Some("executing"), State::LaunchIntent) => completed.is_none(),
            // A retained boot binding may survive a lost later observation.
            ("unknown", Some("reconciling"), State::LaunchIntent | State::Unknown) => {
                completed.is_none()
            }
            ("succeeded", Some("exited"), State::Exited) => {
                completed.is_some() && e.receipt.exit_code == Some(0)
            }
            ("failed", Some("exited"), State::Exited) => {
                completed.is_some() && e.receipt.exit_code != Some(0)
            }
            ("failed", Some("timed_out"), State::TimedOut)
            | ("cancelled", Some("cancelled"), State::Cancelled) => completed.is_some(),
            _ => false,
        };
        if !valid {
            return Err(OutputError::Corrupt);
        }
        let live:bool=sqlx::query_scalar("SELECT a.status='running' AND a.lease_expires_at>clock_timestamp() AND a.released_at IS NULL AND h.supervisor_epoch=a.supervisor_epoch FROM allocations a JOIN hosts h ON h.id=a.host_id WHERE a.id=$1 AND a.project_id=$2 AND a.sandbox_id=$3")
            .bind(e.owner.allocation_id.uuid()).bind(project.uuid()).bind(e.owner.sandbox_id.uuid()).fetch_optional(&mut *tx).await?.unwrap_or(false);
        if !live {
            return Ok(view(StreamSource::Missing));
        }
        let scope = LiveOutputScope {
            version: 1,
            owner: e.owner,
            command_digest: e.receipt.digest,
            output_limit: e.receipt.output_limit,
            deadline_unix_ms: e.receipt.deadline_unix_ms,
        };
        scope.validate().map_err(|_| OutputError::Corrupt)?;
        Ok(view(StreamSource::Live {
            scope,
            simulated: e.simulated,
        }))
    }
}
