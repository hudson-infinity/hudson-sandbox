use super::*;
use sandbox_protocol::supervisor::AllocationMetadataObservation;
use serde::{Deserialize, Serialize};

/// Historical physical metadata completion, not a fresh host claim or proof of
/// root forgetting. Read this after an uncertain database commit before retrying.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Completion {
    pub request: Request,
    pub observed_unix_ms: i64,
    pub completed_at: OffsetDateTime,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Evidence {
    version: u32,
    request: Request,
    observed_unix_ms: i64,
}
pub(super) fn millis(value: OffsetDateTime) -> Result<i64, Error> {
    i64::try_from(value.unix_timestamp_nanos() / 1_000_000).map_err(|_| Error::Evidence)
}
fn release_matches(a: &PgRow, intent: &Intent) -> Result<(), Error> {
    allocation_matches(a, &intent.permit)?;
    let release: Value = a.try_get("release_evidence")?;
    if a.try_get::<String, _>("status")? != "released"
        || a.try_get::<Option<OffsetDateTime>, _>("released_at")?
            .is_none()
        || intent.release_evidence_sha256
            != hex::encode(Sha256::digest(
                serde_json::to_vec(&release).map_err(|_| Error::Evidence)?,
            ))
    {
        return Err(Error::Evidence);
    }
    Ok(())
}
pub(super) fn retained(r: &PgRow, p: &Permit, a: &PgRow) -> Result<Option<Completion>, Error> {
    let value: Option<Value> = r.try_get("metadata_completion")?;
    let completed: Option<OffsetDateTime> = r.try_get("metadata_completed_at")?;
    let expires: Option<OffsetDateTime> = r.try_get("metadata_claim_expires_at")?;
    let Some(value) = value else {
        if completed.is_some() || expires.is_some() {
            return Err(Error::Evidence);
        }
        return Ok(None);
    };
    let e: Evidence = serde_json::from_value(value).map_err(|_| Error::Evidence)?;
    let completed_at = completed.ok_or(Error::Evidence)?;
    let expires_at = expires.ok_or(Error::Evidence)?;
    let intent: Intent =
        serde_json::from_value(r.try_get("intent")?).map_err(|_| Error::Evidence)?;
    intent.validate().map_err(|_| Error::Evidence)?;
    if completed_at >= expires_at
        || e.version != 1
        || e.request.intent != intent
        || intent.permit != *p
        || intent.simulated
        || intent.retirement.uuid() != r.try_get::<uuid::Uuid, _>("retirement_id")?
        || p.allocation.uuid() != r.try_get::<uuid::Uuid, _>("allocation_id")?
        || e.request.revision <= 0
        || e.request.revision != r.try_get::<i64, _>("claim_revision")?
        || e.request.reporting_epoch != r.try_get::<i64, _>("reporting_epoch")?
        || e.request.reporting_epoch < p.original_epoch
        || e.request.expires_unix_ms != millis(expires_at)?
        || r.try_get::<Option<OffsetDateTime>, _>("lease_expires_at")?
            .is_some()
        || e.observed_unix_ms <= 0
        || e.observed_unix_ms > e.request.expires_unix_ms
        || e.observed_unix_ms > millis(completed_at)?.saturating_add(5_000)
        || e.observed_unix_ms < millis(completed_at)?.saturating_sub(10_000)
    {
        return Err(Error::Evidence);
    }
    release_matches(a, &intent)?;
    Ok(Some(Completion {
        request: e.request,
        observed_unix_ms: e.observed_unix_ms,
        completed_at,
    }))
}
fn identical(saved: Completion, request: &Request, observed: i64) -> Result<Completion, Error> {
    if saved.request != *request || saved.observed_unix_ms != observed {
        return Err(Error::Evidence);
    }
    Ok(saved)
}
impl Store {
    /// Historical lookup uses a consistent read-only snapshot and intentionally
    /// survives host epoch advancement and expiry of the completed claim.
    pub async fn allocation_retirement_completion(
        &self,
        intent: &Intent,
    ) -> Result<Option<Completion>, Error> {
        intent.validate().map_err(|_| Error::Evidence)?;
        if intent.simulated {
            return Err(Error::Evidence);
        }
        let mut tx = self.pool().begin().await?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
            .execute(&mut *tx)
            .await?;
        sqlx::query("SET LOCAL statement_timeout='5s'")
            .execute(&mut *tx)
            .await?;
        let Some(r) = sqlx::query("SELECT * FROM allocation_retirements WHERE allocation_id=$1")
            .bind(intent.permit.allocation.uuid())
            .fetch_optional(&mut *tx)
            .await?
        else {
            tx.rollback().await?;
            return Ok(None);
        };
        let frozen: Intent =
            serde_json::from_value(r.try_get("intent")?).map_err(|_| Error::Evidence)?;
        if frozen != *intent {
            return Err(Error::Evidence);
        }
        let a = sqlx::query("SELECT * FROM allocations WHERE id=$1")
            .bind(intent.permit.allocation.uuid())
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(Error::Evidence)?;
        let p = sqlx::query("SELECT * FROM allocation_permits WHERE allocation_id=$1")
            .bind(intent.permit.allocation.uuid())
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(Error::Evidence)?;
        let completion = retained(&r, &permit(&p)?, &a)?;
        tx.rollback().await?;
        Ok(completion)
    }
    /// The caller supplies an observation received from the authenticated,
    /// controller-pinned metadata RPC. Ordinary release/fence responses are not
    /// interchangeable with this type. No simulator completion is accepted.
    pub async fn complete_allocation_retirement(
        &self,
        request: &Request,
        observation: &AllocationMetadataObservation,
    ) -> Result<Completion, Error> {
        let encoded = request.encode().map_err(|_| Error::Evidence)?;
        if request.intent.simulated
            || request.revision <= 0
            || request.reporting_epoch < request.intent.permit.original_epoch
            || request.expires_unix_ms <= 0
            || observation.observed_unix_ms <= 0
            || observation
                .request
                .as_ref()
                .is_none_or(|r| r.request_json != encoded)
        {
            return Err(Error::Evidence);
        }
        if let Some(saved) = self
            .allocation_retirement_completion(&request.intent)
            .await?
        {
            return identical(saved, request, observation.observed_unix_ms);
        }
        let p = &request.intent.permit;
        let mut tx = self.pool().begin().await?;
        sqlx::query("SET LOCAL statement_timeout='5s'")
            .execute(&mut *tx)
            .await?;
        // Lock host before allocation, never in the opposite order. SHARE
        // permits concurrent acknowledgements but serializes epoch/mode changes
        // through commit; a final unlocked EXISTS check would not do that.
        sqlx::query("SELECT id FROM hosts WHERE id=$1 AND supervisor_epoch=$2 AND launch_authority_required AND registered_allocation_serial>=$3 FOR SHARE")
            .bind(p.host.uuid()).bind(request.reporting_epoch).bind(p.serial as i64)
            .fetch_optional(&mut *tx).await?.ok_or(Error::LostClaim)?;
        let a = sqlx::query("SELECT * FROM allocations WHERE id=$1 FOR UPDATE")
            .bind(p.allocation.uuid())
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(Error::LostClaim)?;
        let registered = sqlx::query("SELECT * FROM allocation_permits WHERE allocation_id=$1")
            .bind(p.allocation.uuid())
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(Error::Evidence)?;
        if permit(&registered)? != *p {
            return Err(Error::Evidence);
        }
        allocation_matches(&a, p)?;
        let r =
            sqlx::query("SELECT * FROM allocation_retirements WHERE allocation_id=$1 FOR UPDATE")
                .bind(p.allocation.uuid())
                .fetch_optional(&mut *tx)
                .await?
                .ok_or(Error::LostClaim)?;
        if let Some(saved) = retained(&r, p, &a)? {
            tx.rollback().await?;
            return identical(saved, request, observation.observed_unix_ms);
        }
        let intent: Intent =
            serde_json::from_value(r.try_get("intent")?).map_err(|_| Error::Evidence)?;
        let expires: OffsetDateTime = r
            .try_get::<Option<OffsetDateTime>, _>("lease_expires_at")?
            .ok_or(Error::LostClaim)?;
        if intent != request.intent
            || r.try_get::<uuid::Uuid, _>("retirement_id")? != intent.retirement.uuid()
        {
            return Err(Error::Evidence);
        }
        if request.revision != r.try_get::<i64, _>("claim_revision")?
            || request.reporting_epoch != r.try_get::<i64, _>("reporting_epoch")?
            || request.expires_unix_ms != millis(expires)?
        {
            return Err(Error::LostClaim);
        }
        // Revalidate original release/outcomes and consumer closure under the
        // admission lock; preparation alone is not completion evidence.
        if frozen_scope(
            &mut tx,
            &a,
            p.clone(),
            intent.retirement,
            false,
            request.reporting_epoch,
        )
        .await?
            != intent
        {
            return Err(Error::Evidence);
        }
        let now: OffsetDateTime = sqlx::query_scalar("SELECT clock_timestamp()")
            .fetch_one(&mut *tx)
            .await?;
        request
            .validate(
                &intent,
                request.reporting_epoch,
                request.revision,
                millis(now)?,
            )
            .map_err(|_| Error::LostClaim)?;
        let observed = observation.observed_unix_ms;
        if observed > millis(now)?.saturating_add(5_000)
            || observed < millis(now)?.saturating_sub(10_000)
            || observed > request.expires_unix_ms
        {
            return Err(Error::Evidence);
        }
        let e = Evidence {
            version: 1,
            request: request.clone(),
            observed_unix_ms: observed,
        };
        let completed_at: OffsetDateTime = sqlx::query_scalar("WITH decision AS MATERIALIZED (SELECT clock_timestamp() AS now) UPDATE allocation_retirements SET metadata_completion=$5,metadata_completed_at=decision.now,metadata_claim_expires_at=lease_expires_at,lease_expires_at=NULL FROM decision WHERE allocation_id=$1 AND claim_revision=$2 AND reporting_epoch=$3 AND lease_expires_at=$4 AND lease_expires_at>decision.now AND metadata_completion IS NULL AND $6::bigint BETWEEN floor(extract(epoch FROM decision.now)*1000)::bigint-10000 AND floor(extract(epoch FROM decision.now)*1000)::bigint+5000 RETURNING metadata_completed_at")
            .bind(p.allocation.uuid()).bind(request.revision).bind(request.reporting_epoch).bind(expires)
            .bind(serde_json::to_value(e).map_err(|_|Error::Evidence)?).bind(observed).fetch_optional(&mut *tx).await?.ok_or(Error::LostClaim)?;
        tx.commit().await?;
        Ok(Completion {
            request: request.clone(),
            observed_unix_ms: observed,
            completed_at,
        })
    }
}
