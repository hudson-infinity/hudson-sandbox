//! Current tenant-scoped allocation evidence for read-only file access. Never claims work.
use crate::{Store, StoreError};
use sandbox_protocol::{
    AllocationId, HostId, Id, ProjectId, SandboxId, TokenHash, TokenKeyId,
    file_downloads::ReadScope,
};
use sqlx::Row;
use time::OffsetDateTime;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileView {
    pub scope: ReadScope,
    pub simulated: bool,
    pub lease_expires_at: OffsetDateTime,
    pub sandbox_expires_at: Option<OffsetDateTime>,
    pub authorization_expires_at: Option<OffsetDateTime>,
}
impl FileView {
    pub fn expired(&self) -> bool {
        let now = OffsetDateTime::now_utc();
        self.lease_expires_at <= now
            || self.sandbox_expires_at.is_some_and(|t| t <= now)
            || self.authorization_expires_at.is_some_and(|t| t <= now)
    }
    pub fn same_binding(&self, other: &Self) -> bool {
        self.scope == other.scope && self.simulated == other.simulated
    }
}

const FILE_QUERY: &str = "SELECT a.id AS allocation_id,a.host_id,a.supervisor_epoch,a.generation,s.project_id,s.id AS sandbox_id,
            a.lease_expires_at,s.expires_at,s.observation_simulated
            FROM sandboxes s JOIN projects p ON p.id=s.project_id
            JOIN allocations a ON a.id=s.current_allocation_id AND a.project_id=s.project_id AND a.sandbox_id=s.id
            JOIN hosts h ON h.id=a.host_id
            WHERE s.id=$1 AND s.project_id=$2 AND p.status='active'
            AND s.desired_state='running' AND s.observed_state='running' AND s.destroyed_at IS NULL
            AND s.active_transition_operation_id IS NULL AND s.generation=a.generation
            AND (s.expires_at IS NULL OR s.expires_at>clock_timestamp())
            AND a.status='running' AND a.released_at IS NULL AND a.lease_expires_at>clock_timestamp()
            AND a.supervisor_epoch>0 AND h.supervisor_epoch=a.supervisor_epoch
            AND h.status IN ('ready','draining') AND s.observation_simulated IS NOT NULL";

fn file_view(row: &sqlx::postgres::PgRow) -> Result<FileView, StoreError> {
    let view = FileView {
        scope: ReadScope {
            version: 1,
            host_id: HostId::from_uuid(row.try_get("host_id").map_err(StoreError::Query)?),
            host_epoch: row.try_get("supervisor_epoch").map_err(StoreError::Query)?,
            project_id: ProjectId::from_uuid(row.try_get("project_id").map_err(StoreError::Query)?),
            sandbox_id: SandboxId::from_uuid(row.try_get("sandbox_id").map_err(StoreError::Query)?),
            allocation_id: AllocationId::from_uuid(
                row.try_get("allocation_id").map_err(StoreError::Query)?,
            ),
            generation: row.try_get("generation").map_err(StoreError::Query)?,
        },
        simulated: row
            .try_get("observation_simulated")
            .map_err(StoreError::Query)?,
        lease_expires_at: row.try_get("lease_expires_at").map_err(StoreError::Query)?,
        sandbox_expires_at: row.try_get("expires_at").map_err(StoreError::Query)?,
        authorization_expires_at: None,
    };
    view.scope
        .validate()
        .map_err(|_| StoreError::Corrupt("invalid file read scope".into()))?;
    Ok(view)
}

#[derive(Debug)]
pub enum FileAccess {
    Unauthorized,
    NotFound,
    Ready(FileView),
}
impl Store {
    /// Locks last only for this metadata check, never for guest I/O. Acquiring rows
    /// before the final projections prevents a lock wait from returning a stale
    /// credential/scope combination. A commit error never authorizes byte release.
    pub async fn authorized_file_view(
        &self,
        project: ProjectId,
        sandbox: SandboxId,
        key: &TokenKeyId,
        hash: &TokenHash,
    ) -> Result<FileAccess, StoreError> {
        let mut tx = self.pool().begin().await.map_err(StoreError::Query)?;
        sqlx::query("SET LOCAL statement_timeout='5s'")
            .execute(&mut *tx)
            .await
            .map_err(StoreError::Query)?;
        let project_exists: Option<(uuid::Uuid,)> =
            sqlx::query_as("SELECT id FROM projects WHERE id=$1 FOR SHARE")
                .bind(project.uuid())
                .fetch_optional(&mut *tx)
                .await
                .map_err(StoreError::Query)?;
        if project_exists.is_none() {
            return Ok(FileAccess::Unauthorized);
        }
        let allocation: Option<(Option<uuid::Uuid>,)> = sqlx::query_as(
            "SELECT current_allocation_id FROM sandboxes WHERE id=$1 AND project_id=$2 FOR SHARE",
        )
        .bind(sandbox.uuid())
        .bind(project.uuid())
        .fetch_optional(&mut *tx)
        .await
        .map_err(StoreError::Query)?;
        if let Some((Some(allocation),)) = allocation {
            let host:Option<(uuid::Uuid,)> = sqlx::query_as("SELECT host_id FROM allocations WHERE id=$1 AND project_id=$2 AND sandbox_id=$3 FOR SHARE")
                .bind(allocation).bind(project.uuid()).bind(sandbox.uuid()).fetch_optional(&mut *tx).await.map_err(StoreError::Query)?;
            if let Some((host,)) = host {
                sqlx::query("SELECT id FROM hosts WHERE id=$1 FOR SHARE")
                    .bind(host)
                    .fetch_optional(&mut *tx)
                    .await
                    .map_err(StoreError::Query)?;
            }
        }
        let token=sqlx::query("SELECT p.id AS project_id,p.status AS project_status,t.value->>'key_id' AS key_id,
            decode(t.value->>'hash','hex') AS hash,(t.value->>'expires_at')::timestamptz AS expires_at,
            (t.value->>'revoked_at')::timestamptz AS revoked_at,clock_timestamp() AS checked_at
            FROM projects p CROSS JOIN LATERAL jsonb_array_elements(p.api_tokens) AS t(value)
            WHERE p.id=$1 AND t.value->>'key_id'=$2")
            .bind(project.uuid()).bind(key.as_str()).fetch_optional(&mut *tx).await.map_err(StoreError::Query)?;
        let Some(token) = token else {
            return Ok(FileAccess::Unauthorized);
        };
        let checked: OffsetDateTime = token.try_get("checked_at").map_err(StoreError::Query)?;
        let token = crate::projects::token_record(&token)?;
        if token.project_id != project
            || &token.key_id != key
            || !hash.verify(&token.hash)
            || token.project_status != crate::projects::ProjectStatus::Active
            || token.expires_at.is_some_and(|at| at <= checked)
            || token.revoked_at.is_some_and(|at| at <= checked)
        {
            return Ok(FileAccess::Unauthorized);
        }
        let row = sqlx::query(FILE_QUERY)
            .bind(sandbox.uuid())
            .bind(project.uuid())
            .fetch_optional(&mut *tx)
            .await
            .map_err(StoreError::Query)?;
        let mut view = row.as_ref().map(file_view).transpose()?;
        if let Some(view) = &mut view {
            view.authorization_expires_at = match (token.expires_at, token.revoked_at) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            };
        }
        tx.commit().await.map_err(StoreError::Query)?;
        if token
            .expires_at
            .is_some_and(|at| at <= OffsetDateTime::now_utc())
            || token
                .revoked_at
                .is_some_and(|at| at <= OffsetDateTime::now_utc())
        {
            return Ok(FileAccess::Unauthorized);
        }
        Ok(view.map_or(FileAccess::NotFound, FileAccess::Ready))
    }
}
