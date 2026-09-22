use super::*;

/// A scheduling hint, never authority to delete metadata. Preparation and
/// historical completion lookup must independently validate this allocation.
#[derive(Debug)]
pub struct Candidate {
    pub allocation: AllocationId,
    pub intent: Option<Intent>,
}

impl Store {
    /// Select one released physical allocation and persist its scan time before
    /// interpreting its proof. Malformed or ineligible records therefore cannot
    /// monopolize the worker. Existing delivery leases remain the RPC authority.
    pub async fn next_allocation_retirement(
        &self,
        host: HostId,
        epoch: i64,
    ) -> Result<Option<Candidate>, Error> {
        if epoch <= 0 {
            return Err(Error::Policy);
        }
        let mut tx = self.pool().begin().await?;
        sqlx::query("SET LOCAL statement_timeout='5s'")
            .execute(&mut *tx)
            .await?;
        let row = sqlx::query(
            "SELECT a.id,r.intent FROM allocations a
             JOIN hosts h ON h.id=a.host_id
             JOIN allocation_permits p ON p.allocation_id=a.id AND p.host_id=h.id
             LEFT JOIN allocation_retirements r ON r.allocation_id=a.id
             WHERE a.host_id=$1 AND h.supervisor_epoch=$2
               AND h.launch_authority_required AND h.registered_allocation_serial>=p.serial
               AND a.supervisor_epoch<=$2 AND a.status='released' AND a.released_at IS NOT NULL
               AND r.forget_completion IS NULL
               AND (r.lease_expires_at IS NULL OR r.lease_expires_at<=clock_timestamp())
               AND (r.forget_lease_expires_at IS NULL OR r.forget_lease_expires_at<=clock_timestamp())
               AND (a.retirement_scan_at IS NULL OR a.retirement_scan_at<=clock_timestamp()-interval '5 seconds')
             ORDER BY a.retirement_scan_at NULLS FIRST,a.id
             LIMIT 1 FOR UPDATE OF a SKIP LOCKED",
        )
        .bind(host.uuid())
        .bind(epoch)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(row) = row else {
            tx.rollback().await?;
            return Ok(None);
        };
        let id: uuid::Uuid = row.try_get("id")?;
        let intent: Option<Value> = row.try_get("intent")?;
        sqlx::query("UPDATE allocations SET retirement_scan_at=clock_timestamp() WHERE id=$1")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        let intent: Option<Intent> = intent
            .map(|v| serde_json::from_value(v).map_err(|_| Error::Evidence))
            .transpose()?;
        if let Some(intent) = &intent {
            intent.validate().map_err(|_| Error::Evidence)?;
            if intent.permit.allocation.uuid() != id
                || intent.permit.host != host
                || intent.simulated
            {
                return Err(Error::Evidence);
            }
        }
        Ok(Some(Candidate {
            allocation: AllocationId::from_uuid(id),
            intent,
        }))
    }
}
