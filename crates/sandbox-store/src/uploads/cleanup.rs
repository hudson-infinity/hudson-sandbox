//! Storage-only source retirement. Operation/guest history is never erased here.
use super::*;
use crate::Store;
use sandbox_protocol::file_sources::SourceRetirement;
use serde_json::json;

#[derive(Debug, Clone)]
pub struct SourceCleanupClaim {
    pub operation_id: OperationId,
    pub revision: i64,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceCleanupManifest {
    pub version: u32,
    pub plan: SourcePlan,
    pub selected: Option<SourceRef>,
}
#[derive(Debug)]
pub struct SourceCleanupWork {
    pub manifest: SourceCleanupManifest,
    /// PostgreSQL time, checked after all metadata locks.
    pub now_unix_ms: i64,
}
async fn timeout(db: &mut PgConnection) -> Result<(), DispatchError> {
    sqlx::query("SET LOCAL statement_timeout='2s'")
        .execute(db)
        .await?;
    Ok(())
}
async fn rows(db: &mut PgConnection, id: OperationId) -> Result<(PgRow, PgRow), DispatchError> {
    let op = sqlx::query("SELECT * FROM operations WHERE id=$1 FOR UPDATE")
        .bind(id.uuid())
        .fetch_optional(&mut *db)
        .await?
        .ok_or(DispatchError::InvalidData)?;
    let file = sqlx::query("SELECT * FROM file_uploads WHERE operation_id=$1 FOR UPDATE")
        .bind(id.uuid())
        .fetch_optional(&mut *db)
        .await?
        .ok_or(DispatchError::InvalidData)?;
    Ok((op, file))
}
async fn manifest(
    db: &mut PgConnection,
    op: &PgRow,
    file: &PgRow,
) -> Result<SourceCleanupManifest, DispatchError> {
    let (plan, selected) = source_identity(db, op, file).await?;
    let current = SourceCleanupManifest {
        version: 1,
        plan,
        selected,
    };
    let saved: SourceCleanupManifest =
        serde_json::from_value(file.try_get("source_cleanup_manifest")?)
            .map_err(|_| DispatchError::InvalidData)?;
    if saved != current
        || file
            .try_get::<Option<OffsetDateTime>, _>("source_frozen_at")?
            .is_none()
    {
        return Err(DispatchError::InvalidData);
    }
    Ok(saved)
}
async fn fence(
    db: &mut PgConnection,
    claim: &SourceCleanupClaim,
    plan: &SourcePlan,
) -> Result<i64, DispatchError> {
    let now: Option<OffsetDateTime> = sqlx::query_scalar(
        "SELECT clock_timestamp() FROM file_uploads WHERE operation_id=$1 AND source_cleanup_revision=$2
         AND source_frozen_at IS NOT NULL AND source_retired_at IS NULL
         AND source_cleanup_lease_until>clock_timestamp()
         AND extract(epoch FROM clock_timestamp())*1000 >= $3::bigint
         AND EXISTS(SELECT 1 FROM operations o WHERE o.id=file_uploads.operation_id
             AND o.deadline<=clock_timestamp()-interval '5 minutes')")
        .bind(claim.operation_id.uuid()).bind(claim.revision).bind(plan.delete_after_unix_ms)
        .fetch_optional(db).await?;
    ms(now.ok_or(DispatchError::LostClaim)?)
}
impl Store {
    /// Freeze one expired source after any live operation lease ends, then claim
    /// storage cleanup separately. Wait past the operation deadline plus the maximum
    /// 300-second issued claim, including replies that cleared the DB lease early.
    /// A freeze grants no guest mutation authority.
    pub async fn claim_file_source_cleanup(
        &self,
        seconds: u32,
    ) -> Result<Option<SourceCleanupClaim>, DispatchError> {
        if !(1..=300).contains(&seconds) {
            return Err(DispatchError::Conflict);
        }
        let mut tx = self.pool().begin().await?;
        timeout(&mut tx).await?;
        let id: Option<uuid::Uuid> = sqlx::query_scalar(
            "SELECT o.id FROM operations o JOIN file_uploads f ON f.operation_id=o.id
             WHERE f.source_retired_at IS NULL
             AND o.deadline<=clock_timestamp()-interval '5 minutes'
             AND (f.source_cleanup_lease_until IS NULL OR f.source_cleanup_lease_until<=clock_timestamp())
             AND (f.source_cleanup_next_at IS NULL OR f.source_cleanup_next_at<=clock_timestamp())
             AND (f.source_frozen_at IS NOT NULL OR o.lease_expires_at IS NULL OR o.lease_expires_at<=clock_timestamp())
             AND (f.plan->>'delete_after_unix_ms')::numeric <= extract(epoch FROM clock_timestamp())*1000
             ORDER BY o.created_at,o.id FOR UPDATE OF o SKIP LOCKED LIMIT 1")
            .fetch_optional(&mut *tx).await?;
        let Some(id) = id else { return Ok(None) };
        let id = OperationId::from_uuid(id);
        let (op, file) = rows(&mut tx, id).await?;
        // Recheck both claims after waiting for the file row and original allocation.
        let (plan, selected) = source_identity(&mut tx, &op, &file).await?;
        let frozen = file
            .try_get::<Option<OffsetDateTime>, _>("source_frozen_at")?
            .is_some();
        let saved = if frozen {
            manifest(&mut tx, &op, &file).await?
        } else {
            SourceCleanupManifest {
                version: 1,
                plan,
                selected,
            }
        };
        let revision:Option<i64>=sqlx::query_scalar(
            "UPDATE file_uploads f SET source_frozen_at=COALESCE(source_frozen_at,clock_timestamp()),
             source_cleanup_manifest=COALESCE(source_cleanup_manifest,$2),source_cleanup_revision=source_cleanup_revision+1,
             source_cleanup_lease_until=clock_timestamp()+make_interval(secs=>$3),source_cleanup_next_at=NULL
             WHERE operation_id=$1 AND source_retired_at IS NULL
             AND (source_cleanup_lease_until IS NULL OR source_cleanup_lease_until<=clock_timestamp())
             AND (source_cleanup_next_at IS NULL OR source_cleanup_next_at<=clock_timestamp())
             AND extract(epoch FROM clock_timestamp())*1000 >= $4::bigint
             AND EXISTS(SELECT 1 FROM operations o WHERE o.id=f.operation_id
                 AND o.deadline<=clock_timestamp()-interval '5 minutes')
             AND (source_frozen_at IS NOT NULL OR EXISTS(SELECT 1 FROM operations o WHERE o.id=f.operation_id
                  AND (o.lease_expires_at IS NULL OR o.lease_expires_at<=clock_timestamp())))
             RETURNING source_cleanup_revision")
            .bind(id.uuid()).bind(json!(saved)).bind(seconds as i32).bind(saved.plan.delete_after_unix_ms)
            .fetch_optional(&mut *tx).await?;
        let Some(revision) = revision else {
            return Ok(None);
        };
        tx.commit().await?;
        Ok(Some(SourceCleanupClaim {
            operation_id: id,
            revision,
        }))
    }
    pub async fn prepare_file_source_cleanup(
        &self,
        claim: &SourceCleanupClaim,
    ) -> Result<SourceCleanupWork, DispatchError> {
        let mut tx = self.pool().begin().await?;
        timeout(&mut tx).await?;
        let (op, file) = rows(&mut tx, claim.operation_id).await?;
        let manifest = manifest(&mut tx, &op, &file).await?;
        let now_unix_ms = fence(&mut tx, claim, &manifest.plan).await?;
        tx.commit().await?;
        Ok(SourceCleanupWork {
            manifest,
            now_unix_ms,
        })
    }
    /// Trusted worker completion after full storage verification. A receipt is
    /// never accepted from a public API caller. Only source byte accounting changes.
    pub async fn complete_file_source_cleanup(
        &self,
        claim: &SourceCleanupClaim,
        receipt: &SourceRetirement,
    ) -> Result<(), DispatchError> {
        let mut tx = self.pool().begin().await?;
        timeout(&mut tx).await?;
        let (op, file) = rows(&mut tx, claim.operation_id).await?;
        let m = manifest(&mut tx, &op, &file).await?;
        receipt
            .validate(&m.plan)
            .map_err(|_| DispatchError::BadEvidence)?;
        if m.selected
            .as_ref()
            .is_some_and(|r| receipt.previous.as_ref() != Some(r))
        {
            return Err(DispatchError::BadEvidence);
        }
        if file
            .try_get::<Option<OffsetDateTime>, _>("source_retired_at")?
            .is_some()
        {
            let retained: SourceRetirement =
                serde_json::from_value(file.try_get("source_retirement")?)
                    .map_err(|_| DispatchError::InvalidData)?;
            if file.try_get::<i64, _>("source_cleanup_revision")? != claim.revision {
                return Err(DispatchError::LostClaim);
            }
            if &retained != receipt {
                return Err(DispatchError::BadEvidence);
            }
            // Reconcile a lost completion acknowledgement without writing or refunding twice.
            tx.commit().await?;
            return Ok(());
        }
        fence(&mut tx, claim, &m.plan).await?;
        let changed=sqlx::query("UPDATE file_uploads SET source_retired_at=clock_timestamp(),source_retirement=$3,
            source_cleanup_lease_until=NULL,source_cleanup_next_at=NULL WHERE operation_id=$1
            AND source_cleanup_revision=$2 AND source_retired_at IS NULL AND source_cleanup_lease_until>clock_timestamp()")
            .bind(claim.operation_id.uuid()).bind(claim.revision).bind(json!(receipt)).execute(&mut *tx).await?.rows_affected();
        if changed != 1 {
            return Err(DispatchError::LostClaim);
        }
        tx.commit().await?;
        Ok(())
    }
    pub async fn defer_file_source_cleanup(
        &self,
        claim: &SourceCleanupClaim,
        seconds: u32,
    ) -> Result<(), DispatchError> {
        if !(1..=3600).contains(&seconds) {
            return Err(DispatchError::Conflict);
        }
        let changed=sqlx::query("UPDATE file_uploads SET source_cleanup_lease_until=NULL,
            source_cleanup_next_at=clock_timestamp()+make_interval(secs=>$3) WHERE operation_id=$1
            AND source_cleanup_revision=$2 AND source_cleanup_lease_until>clock_timestamp() AND source_retired_at IS NULL")
            .bind(claim.operation_id.uuid()).bind(claim.revision).bind(seconds as i32).execute(self.pool()).await?.rows_affected();
        if changed != 1 {
            return Err(DispatchError::LostClaim);
        }
        Ok(())
    }
}

/// Read-only verification for allocation history. The caller owns the allocation
/// admission lock; do not acquire operation/file locks in the reverse order here.
pub(crate) async fn validate_retired(
    db: &mut PgConnection,
    op: &PgRow,
    file: &PgRow,
) -> Result<SourcePlan, DispatchError> {
    let saved = manifest(db, op, file).await?;
    if file
        .try_get::<Option<OffsetDateTime>, _>("source_retired_at")?
        .is_none()
        || file
            .try_get::<Option<OffsetDateTime>, _>("source_cleanup_lease_until")?
            .is_some()
    {
        return Err(DispatchError::InvalidData);
    }
    let receipt: SourceRetirement = serde_json::from_value(file.try_get("source_retirement")?)
        .map_err(|_| DispatchError::InvalidData)?;
    receipt
        .validate(&saved.plan)
        .map_err(|_| DispatchError::InvalidData)?;
    if receipt.previous != saved.selected {
        return Err(DispatchError::InvalidData);
    }
    Ok(saved.plan)
}
