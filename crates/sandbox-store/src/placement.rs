//! Single-host reservations. No method here starts a VM or releases capacity.
//!
//! Lock order is operation -> project -> sandbox -> host. All placement writers
//! must take the same project and host locks before reading aggregate usage.
//! Existing reservations always require reconciliation, even when a lease expired.

use sandbox_protocol::{AllocationId, HostId, Id};
use serde_json::{Value, json};
use sqlx::Row;

use crate::{Store, claims::Claim};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Allocation {
    pub id: AllocationId,
    pub host_id: HostId,
    pub generation: i64,
    pub supervisor_epoch: i64,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Reservation {
    /// Capacity is reserved; boot has not been dispatched or confirmed.
    Reserved(Allocation),
    /// Reconcile this incarnation instead of allocating or blindly starting another.
    Existing(Allocation),
}

#[derive(Debug, thiserror::Error)]
pub enum PlacementError {
    #[error("operation claim expired or changed")]
    LostClaim,
    #[error("current project credential does not permit execution")]
    Unauthorized,
    #[error("sandbox requires reconciliation before placement")]
    Reconcile,
    #[error("host is not ready, fresh, or at the expected epoch")]
    HostUnavailable,
    #[error("insufficient host capacity")]
    Capacity,
    #[error("project allocation quota exceeded")]
    Quota,
    #[error("invalid resource or quota metadata")]
    InvalidResources,
    #[error("placement storage query failed: {0}")]
    Query(#[from] sqlx::Error),
}

fn positive(value: &Value, key: &str) -> Result<i64, PlacementError> {
    value
        .get(key)
        .and_then(Value::as_i64)
        .filter(|v| *v > 0)
        .ok_or(PlacementError::InvalidResources)
}

fn limit(value: &Value, key: &str, default: i64) -> Result<i64, PlacementError> {
    match value.get(key) {
        None => Ok(default),
        Some(v) => v
            .as_i64()
            .filter(|v| *v >= 0)
            .ok_or(PlacementError::InvalidResources),
    }
}

impl Store {
    /// Reserve CPU/RAM/disk on a single registered host. This is a storage
    /// primitive, not a scheduler: an authenticated host registration and
    /// operator-verified image compatibility are prerequisites at the caller.
    /// Reauthorization here does not replace a fresh check before dispatch.
    pub async fn reserve_create(
        &self,
        claim: &Claim,
        host_id: HostId,
        expected_epoch: i64,
    ) -> Result<Reservation, PlacementError> {
        let mut tx = self.pool().begin().await?;
        let op = sqlx::query(
            "SELECT * FROM operations WHERE id=$1 AND claim_revision=$2
             AND lease_expires_at > clock_timestamp() AND status IN ('running','unknown')
             FOR UPDATE",
        )
        .bind(claim.operation_id.uuid())
        .bind(claim.revision)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(PlacementError::LostClaim)?;
        let project_id: uuid::Uuid = op.try_get("project_id")?;
        let sandbox_id: uuid::Uuid = op.try_get("sandbox_id")?;
        let project = sqlx::query("SELECT * FROM projects WHERE id=$1 FOR UPDATE")
            .bind(project_id)
            .fetch_one(&mut *tx)
            .await?;
        let sandbox = sqlx::query("SELECT * FROM sandboxes WHERE id=$1 FOR UPDATE")
            .bind(sandbox_id)
            .fetch_one(&mut *tx)
            .await?;
        if op.try_get::<String, _>("kind")? != "create"
            || sandbox.try_get::<Option<uuid::Uuid>, _>("active_transition_operation_id")?
                != Some(claim.operation_id.uuid())
        {
            return Err(PlacementError::Reconcile);
        }

        // Resolve an existing incarnation before checking today's authority or
        // capacity. A revoked caller still needs service-owned reconciliation.
        if let Some(id) = sandbox.try_get::<Option<uuid::Uuid>, _>("current_allocation_id")? {
            let row = sqlx::query("SELECT * FROM allocations WHERE id=$1 AND released_at IS NULL")
                .bind(id)
                .fetch_optional(&mut *tx)
                .await?
                .ok_or(PlacementError::Reconcile)?;
            let allocation = Allocation {
                id: AllocationId::from_uuid(id),
                host_id: HostId::from_uuid(row.try_get("host_id")?),
                generation: row.try_get("generation")?,
                supervisor_epoch: row.try_get("supervisor_epoch")?,
            };
            tx.commit().await?;
            return Ok(Reservation::Existing(allocation));
        }
        if op.try_get::<String, _>("status")? == "unknown"
            || op.try_get::<i32, _>("attempt_count")? != 0
            || sandbox.try_get::<i64, _>("generation")? != 0
            || sandbox.try_get::<String, _>("observed_state")? != "creating"
            || sandbox.try_get::<String, _>("desired_state")? != "running"
        {
            return Err(PlacementError::Reconcile);
        }
        let resources: Value = sandbox.try_get("resources")?;
        let cpu = positive(&resources, "vcpu")?;
        let memory = positive(&resources, "memory_mib")?;
        let disk = positive(&resources, "disk_mib")?;
        if cpu > 4 || memory > 8192 || disk > 65536 {
            return Err(PlacementError::InvalidResources);
        }
        let limits: Value = project.try_get("limits")?;
        let project_usage: (i64, i64, i64, i64) = sqlx::query_as(
            "SELECT count(*), COALESCE(sum(vcpu),0)::bigint,
             COALESCE(sum(memory_mib),0)::bigint, COALESCE(sum(disk_mib),0)::bigint
             FROM allocations WHERE project_id=$1 AND released_at IS NULL",
        )
        .bind(project_id)
        .fetch_one(&mut *tx)
        .await?;
        if project_usage.0 >= limit(&limits, "sandboxes", 25)?
            || cpu > limit(&limits, "vcpu", 100)?.saturating_sub(project_usage.1)
            || memory > limit(&limits, "memory_mib", 204800)?.saturating_sub(project_usage.2)
            || disk > limit(&limits, "disk_mib", 1638400)?.saturating_sub(project_usage.3)
        {
            return Err(PlacementError::Quota);
        }
        let host = sqlx::query("SELECT * FROM hosts WHERE id=$1 FOR UPDATE")
            .bind(host_id.uuid())
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(PlacementError::HostUnavailable)?;
        // Time predicates run after acquiring every row lock. A host observation
        // or credential may expire while another transaction holds the host.
        let ready: (bool,) = sqlx::query_as(
            "SELECT status='ready' AND supervisor_epoch=$2 AND supervisor_epoch > 0
             AND COALESCE(last_seen_at > clock_timestamp()-interval '30 seconds',false)
             FROM hosts WHERE id=$1",
        )
        .bind(host_id.uuid())
        .bind(expected_epoch)
        .fetch_one(&mut *tx)
        .await?;
        if !ready.0 {
            return Err(PlacementError::HostUnavailable);
        }
        // Use persisted initiator identity, never the claim object's project fields.
        let authorized: (bool,) = sqlx::query_as(
            "SELECT p.status='active' AND o.initiator_kind='project'
             AND (o.deadline IS NULL OR o.deadline > clock_timestamp())
             AND (s.expires_at IS NULL OR s.expires_at > clock_timestamp())
             AND EXISTS (SELECT 1 FROM jsonb_array_elements(p.api_tokens) t
                WHERE t->>'key_id'=o.initiator_key_id
                AND (t->>'revoked_at' IS NULL OR (t->>'revoked_at')::timestamptz > clock_timestamp())
                AND (t->>'expires_at' IS NULL OR (t->>'expires_at')::timestamptz > clock_timestamp()))
             FROM projects p JOIN operations o ON o.project_id=p.id
             JOIN sandboxes s ON s.id=o.sandbox_id WHERE o.id=$1",
        ).bind(claim.operation_id.uuid()).fetch_one(&mut *tx).await?;
        if !authorized.0 {
            return Err(PlacementError::Unauthorized);
        }

        let usage: (i64, i64, i64) = sqlx::query_as(
            "SELECT COALESCE(sum(vcpu),0)::bigint,COALESCE(sum(memory_mib),0)::bigint,
             COALESCE(sum(disk_mib),0)::bigint FROM allocations
             WHERE host_id=$1 AND released_at IS NULL",
        )
        .bind(host_id.uuid())
        .fetch_one(&mut *tx)
        .await?;
        if cpu > i64::from(host.try_get::<i32, _>("cpu_capacity")?).saturating_sub(usage.0)
            || memory
                > host
                    .try_get::<i64, _>("memory_capacity_mib")?
                    .saturating_sub(usage.1)
            || disk
                > host
                    .try_get::<i64, _>("disk_capacity_mib")?
                    .saturating_sub(usage.2)
        {
            return Err(PlacementError::Capacity);
        }
        let allocation = Allocation {
            id: AllocationId::generate(),
            host_id,
            generation: 1,
            supervisor_epoch: expected_epoch,
        };
        sqlx::query(
            "INSERT INTO allocations(id,project_id,sandbox_id,host_id,generation,supervisor_epoch,
             vcpu,memory_mib,disk_mib,status) VALUES($1,$2,$3,$4,1,$5,$6,$7,$8,'reserved')",
        )
        .bind(allocation.id.uuid())
        .bind(project_id)
        .bind(sandbox_id)
        .bind(host_id.uuid())
        .bind(expected_epoch)
        .bind(cpu as i32)
        .bind(memory)
        .bind(disk)
        .execute(&mut *tx)
        .await?;
        sqlx::query("UPDATE sandboxes SET current_allocation_id=$1,generation=1,state_revision=state_revision+1,updated_at=clock_timestamp() WHERE id=$2")
            .bind(allocation.id.uuid()).bind(sandbox_id).execute(&mut *tx).await?;
        // Check the lease again after potentially waiting for quota/host locks.
        // If it expired during placement, the entire reservation rolls back.
        let changed = sqlx::query(
            "UPDATE operations SET phase='reserved',updated_at=clock_timestamp(),
             attempt_receipts=attempt_receipts || jsonb_build_array($3::jsonb)
             WHERE id=$1 AND claim_revision=$2 AND lease_expires_at > clock_timestamp()",
        )
        .bind(claim.operation_id.uuid())
        .bind(claim.revision)
        .bind(
            json!({"phase":"reserved","allocation_id":allocation.id.to_string(),
                "generation":1,"supervisor_epoch":expected_epoch,"claim_revision":claim.revision}),
        )
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if changed != 1 {
            return Err(PlacementError::LostClaim);
        }
        tx.commit().await?;
        Ok(Reservation::Reserved(allocation))
    }
}
