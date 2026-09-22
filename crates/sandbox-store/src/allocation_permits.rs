//! Stable host-local serials issued inside reservation, never inferred from time.
use sandbox_protocol::{
    AllocationId, HostId, Id, OperationId, ProjectId, SandboxId,
    allocation_authority::{MAX_BATCH, MAX_SERIAL, Permit},
};
use sqlx::Row;

use crate::Store;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("allocation permit evidence invalid: {0}")]
    Corrupt(String),
    #[error("allocation permit query failed: {0}")]
    Query(#[from] sqlx::Error),
}

#[derive(Debug, Clone)]
pub struct PermitBatch {
    pub issued_through: u64,
    pub permits: Vec<Permit>,
    /// Missing or inconsistent permits block activation of the new host authority.
    pub has_unissued_allocations: bool,
}

impl Store {
    /// Read the next bounded registration batch. `after` must ultimately come
    /// from a verified host registration frontier, not an arbitrary client.
    /// This read is not itself a registration acknowledgement or launch grant.
    pub async fn allocation_permits_after(
        &self,
        host: HostId,
        after: u64,
    ) -> Result<PermitBatch, Error> {
        if after > MAX_SERIAL {
            return Err(Error::Corrupt("allocation serial out of range".into()));
        }
        let mut tx = self.pool().begin().await?;
        // Issuance uses the same host lock after operation/project/sandbox locks.
        // This reader takes no further row locks, avoiding that lock-order cycle.
        let through: i64 =
            sqlx::query_scalar("SELECT last_allocation_serial FROM hosts WHERE id=$1 FOR SHARE")
                .bind(host.uuid())
                .fetch_one(&mut *tx)
                .await?;
        if through < 0 || after > through as u64 {
            return Err(Error::Corrupt("allocation frontier mismatch".into()));
        }
        let rows = sqlx::query(
            "SELECT p.*, a.host_id=p.host_id AND a.project_id=p.project_id
             AND a.sandbox_id=p.sandbox_id AND a.generation=p.generation
             AND a.supervisor_epoch=p.original_epoch AND o.kind='create'
             AND o.project_id=p.project_id AND o.sandbox_id=p.sandbox_id AS valid
             FROM allocation_permits p JOIN allocations a ON a.id=p.allocation_id
             JOIN operations o ON o.id=p.create_operation_id
             WHERE p.host_id=$1 AND p.serial>$2 ORDER BY p.serial LIMIT $3",
        )
        .bind(host.uuid())
        .bind(after as i64)
        .bind(MAX_BATCH as i64)
        .fetch_all(&mut *tx)
        .await?;
        let expected = (through as u64 - after).min(MAX_BATCH as u64) as usize;
        if rows.len() != expected {
            return Err(Error::Corrupt("allocation registration gap".into()));
        }
        let mut permits = Vec::with_capacity(rows.len());
        for (index, row) in rows.iter().enumerate() {
            let serial: i64 = row.try_get("serial")?;
            if !row.try_get::<bool, _>("valid")? || serial as u64 != after + index as u64 + 1 {
                return Err(Error::Corrupt("allocation permit mismatch".into()));
            }
            permits.push(Permit {
                host,
                project: ProjectId::from_uuid(row.try_get("project_id")?),
                sandbox: SandboxId::from_uuid(row.try_get("sandbox_id")?),
                allocation: AllocationId::from_uuid(row.try_get("allocation_id")?),
                create_operation: OperationId::from_uuid(row.try_get("create_operation_id")?),
                generation: row.try_get("generation")?,
                original_epoch: row.try_get("original_epoch")?,
                serial: serial as u64,
            });
        }
        let legacy: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM allocations a WHERE a.host_id=$1
             AND NOT EXISTS(SELECT 1 FROM allocation_permits p
               JOIN operations o ON o.id=p.create_operation_id
               WHERE p.allocation_id=a.id AND p.host_id=a.host_id
               AND p.project_id=a.project_id AND p.sandbox_id=a.sandbox_id
               AND p.generation=a.generation AND p.original_epoch=a.supervisor_epoch
               AND o.kind='create' AND o.project_id=a.project_id
               AND o.sandbox_id=a.sandbox_id))",
        )
        .bind(host.uuid())
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(PermitBatch {
            issued_through: through as u64,
            permits,
            has_unissued_allocations: legacy,
        })
    }
}

impl Store {
    /// Persist authenticated host progress. A downgrade or rollback never lowers
    /// this independent checkpoint, including across controller restarts.
    pub async fn record_allocation_registration(
        &self,
        host: HostId,
        epoch: i64,
        required: bool,
        through: u64,
    ) -> Result<(), Error> {
        if epoch <= 0 || through > MAX_SERIAL || (!required && through != 0) {
            return Err(Error::Corrupt("invalid registration checkpoint".into()));
        }
        let changed = sqlx::query("UPDATE hosts SET launch_authority_required=$3, registered_allocation_serial=$4 WHERE id=$1 AND supervisor_epoch=$2 AND (NOT launch_authority_required OR $3) AND registered_allocation_serial<=$4 AND last_allocation_serial>=$4")
            .bind(host.uuid()).bind(epoch).bind(required).bind(through as i64).execute(self.pool()).await?.rows_affected();
        if changed != 1 {
            return Err(Error::Corrupt(
                "registration rollback, downgrade or stale host".into(),
            ));
        }
        Ok(())
    }
    /// Resolve only the immutable permit associated with this exact Create owner.
    pub async fn allocation_launch_permit(
        &self,
        owner: &sandbox_protocol::supervisor::Ownership,
    ) -> Result<Option<Permit>, Error> {
        let bad = || Error::Corrupt("invalid create owner".into());
        let host: HostId = owner.host_id.parse().map_err(|_| bad())?;
        let project: ProjectId = owner.project_id.parse().map_err(|_| bad())?;
        let sandbox: SandboxId = owner.sandbox_id.parse().map_err(|_| bad())?;
        let allocation: AllocationId = owner.allocation_id.parse().map_err(|_| bad())?;
        let create_operation: OperationId = owner.operation_id.parse().map_err(|_| bad())?;
        let serial: Option<i64> = sqlx::query_scalar("SELECT p.serial FROM allocation_permits p JOIN allocations a ON a.id=p.allocation_id JOIN operations o ON o.id=p.create_operation_id WHERE p.host_id=$1 AND p.project_id=$2 AND p.sandbox_id=$3 AND p.allocation_id=$4 AND p.create_operation_id=$5 AND p.generation=$6 AND p.original_epoch=$7 AND a.host_id=p.host_id AND a.project_id=p.project_id AND a.sandbox_id=p.sandbox_id AND a.generation=p.generation AND a.supervisor_epoch=p.original_epoch AND o.kind='create' AND o.project_id=p.project_id AND o.sandbox_id=p.sandbox_id")
            .bind(host.uuid()).bind(project.uuid()).bind(sandbox.uuid()).bind(allocation.uuid()).bind(create_operation.uuid()).bind(owner.generation).bind(owner.supervisor_epoch).fetch_optional(self.pool()).await?;
        Ok(serial.map(|serial| Permit {
            host,
            project,
            sandbox,
            allocation,
            create_operation,
            generation: owner.generation,
            original_epoch: owner.supervisor_epoch,
            serial: serial as u64,
        }))
    }
}
