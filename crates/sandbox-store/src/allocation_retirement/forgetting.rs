use super::completion::millis;
use super::*;
use sandbox_protocol::{
    allocation_retirement::ForgetRequest,
    supervisor::{AllocationForgetObservation, AllocationForgetState},
};
use serde::{Deserialize, Serialize};
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForgettingCompletion {
    pub request: ForgetRequest,
    pub state: AllocationForgetState,
    pub observed_unix_ms: i64,
    pub completed_at: OffsetDateTime,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Evidence {
    version: u32,
    request: ForgetRequest,
    state: i32,
    observed_unix_ms: i64,
}
fn state(value: i32) -> Result<AllocationForgetState, Error> {
    match AllocationForgetState::try_from(value) {
        Ok(s @ (AllocationForgetState::Forgotten | AllocationForgetState::Retired)) => Ok(s),
        _ => Err(Error::Evidence),
    }
}
fn retained(r: &PgRow, p: &Permit, a: &PgRow) -> Result<Option<ForgettingCompletion>, Error> {
    let value: Option<Value> = r.try_get("forget_completion")?;
    let at: Option<OffsetDateTime> = r.try_get("forgotten_at")?;
    let expires: Option<OffsetDateTime> = r.try_get("forget_claim_expires_at")?;
    let Some(value) = value else {
        if at.is_some() || expires.is_some() {
            return Err(Error::Evidence);
        }
        return Ok(None);
    };
    let metadata = completion::retained(r, p, a)?.ok_or(Error::Evidence)?;
    let e: Evidence = serde_json::from_value(value).map_err(|_| Error::Evidence)?;
    e.request.encode().map_err(|_| Error::Evidence)?;
    let at = at.ok_or(Error::Evidence)?;
    let expires = expires.ok_or(Error::Evidence)?;
    if at >= expires
        || e.version != 1
        || e.request.metadata_request != metadata.request
        || e.request.claim.revision != r.try_get::<i64, _>("forget_revision")?
        || Some(e.request.claim.reporting_epoch) != r.try_get::<Option<i64>, _>("forget_epoch")?
        || e.request.claim.expires_unix_ms != millis(expires)?
        || r.try_get::<Option<OffsetDateTime>, _>("forget_lease_expires_at")?
            .is_some()
        || e.observed_unix_ms <= 0
        || e.observed_unix_ms > e.request.claim.expires_unix_ms
        || e.observed_unix_ms < millis(at)?.saturating_sub(10_000)
        || e.observed_unix_ms > millis(at)?.saturating_add(5_000)
    {
        return Err(Error::Evidence);
    }
    Ok(Some(ForgettingCompletion {
        request: e.request,
        state: state(e.state)?,
        observed_unix_ms: e.observed_unix_ms,
        completed_at: at,
    }))
}
fn identical(
    saved: ForgettingCompletion,
    request: &ForgetRequest,
    o: &AllocationForgetObservation,
) -> Result<ForgettingCompletion, Error> {
    if saved.request != *request
        || saved.state as i32 != o.state
        || saved.observed_unix_ms != o.observed_unix_ms
    {
        return Err(Error::Evidence);
    }
    Ok(saved)
}
async fn rows(
    db: &mut PgConnection,
    allocation: AllocationId,
    locked: bool,
) -> Result<(PgRow, Permit, PgRow), Error> {
    let a = sqlx::query(if locked {
        "SELECT * FROM allocations WHERE id=$1 FOR UPDATE"
    } else {
        "SELECT * FROM allocations WHERE id=$1"
    })
    .bind(allocation.uuid())
    .fetch_optional(&mut *db)
    .await?
    .ok_or(Error::Ineligible)?;
    let p = sqlx::query("SELECT * FROM allocation_permits WHERE allocation_id=$1")
        .bind(allocation.uuid())
        .fetch_optional(&mut *db)
        .await?
        .ok_or(Error::Ineligible)?;
    let p = permit(&p)?;
    allocation_matches(&a, &p)?;
    let r = sqlx::query(if locked {
        "SELECT * FROM allocation_retirements WHERE allocation_id=$1 FOR UPDATE"
    } else {
        "SELECT * FROM allocation_retirements WHERE allocation_id=$1"
    })
    .bind(allocation.uuid())
    .fetch_optional(db)
    .await?
    .ok_or(Error::Ineligible)?;
    Ok((a, p, r))
}
impl Store {
    /// Fresh delivery ownership only after exact metadata completion is retained.
    pub async fn prepare_allocation_forgetting(
        &self,
        allocation: AllocationId,
        host: HostId,
        epoch: i64,
        seconds: u32,
    ) -> Result<Option<ForgetRequest>, Error> {
        if epoch <= 0 || !(1..=300).contains(&seconds) {
            return Err(Error::Policy);
        }
        let mut tx = self.pool().begin().await?;
        sqlx::query("SET LOCAL statement_timeout='5s'")
            .execute(&mut *tx)
            .await?;
        let frontier:i64=sqlx::query_scalar("SELECT registered_allocation_serial FROM hosts WHERE id=$1 AND supervisor_epoch=$2 AND launch_authority_required FOR SHARE")
            .bind(host.uuid()).bind(epoch).fetch_optional(&mut *tx).await?.ok_or(Error::Ineligible)?;
        let (a, p, r) = rows(&mut tx, allocation, true).await?;
        if p.host != host || p.original_epoch > epoch || p.serial > frontier as u64 {
            return Err(Error::Evidence);
        }
        let metadata = completion::retained(&r, &p, &a)?.ok_or(Error::Ineligible)?;
        if retained(&r, &p, &a)?.is_some() {
            tx.rollback().await?;
            return Ok(None);
        }
        let now: OffsetDateTime = sqlx::query_scalar("SELECT clock_timestamp()")
            .fetch_one(&mut *tx)
            .await?;
        if r.try_get::<Option<OffsetDateTime>, _>("forget_lease_expires_at")?
            .is_some_and(|t| t > now)
        {
            tx.rollback().await?;
            return Ok(None);
        }
        if frozen_scope(
            &mut tx,
            &a,
            p,
            metadata.request.intent.retirement,
            false,
            epoch,
        )
        .await?
            != metadata.request.intent
        {
            return Err(Error::Evidence);
        }
        let row=sqlx::query("UPDATE allocation_retirements SET forget_revision=forget_revision+1,forget_epoch=$2,forget_lease_expires_at=clock_timestamp()+make_interval(secs=>$3) WHERE allocation_id=$1 AND forget_revision<9223372036854775807 RETURNING forget_revision,forget_lease_expires_at")
            .bind(allocation.uuid()).bind(epoch).bind(f64::from(seconds)).fetch_optional(&mut *tx).await?.ok_or(Error::Policy)?;
        let claim = Request {
            intent: metadata.request.intent.clone(),
            reporting_epoch: epoch,
            revision: row.try_get("forget_revision")?,
            expires_unix_ms: millis(row.try_get("forget_lease_expires_at")?)?,
        };
        let request = ForgetRequest {
            version: 1,
            claim,
            metadata_request: metadata.request,
        };
        request.encode().map_err(|_| Error::Evidence)?;
        tx.commit().await?;
        Ok(Some(request))
    }
    pub async fn allocation_forgetting_completion(
        &self,
        intent: &Intent,
    ) -> Result<Option<ForgettingCompletion>, Error> {
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
        let (a, p, r) = match rows(&mut tx, intent.permit.allocation, false).await {
            Ok(rows) => rows,
            Err(Error::Ineligible) => {
                tx.rollback().await?;
                return Ok(None);
            }
            Err(e) => return Err(e),
        };
        let frozen: Intent =
            serde_json::from_value(r.try_get("intent")?).map_err(|_| Error::Evidence)?;
        if frozen != *intent {
            return Err(Error::Evidence);
        }
        let result = retained(&r, &p, &a)?;
        tx.rollback().await?;
        Ok(result)
    }
    /// Retired is only serial denial. It is consumable here because the exact
    /// original metadata completion is independently retained and revalidated.
    pub async fn complete_allocation_forgetting(
        &self,
        request: &ForgetRequest,
        o: &AllocationForgetObservation,
    ) -> Result<ForgettingCompletion, Error> {
        let encoded = request.encode().map_err(|_| Error::Evidence)?;
        state(o.state)?;
        if o.observed_unix_ms <= 0 || o.request.as_ref().is_none_or(|w| w.request_json != encoded) {
            return Err(Error::Evidence);
        }
        if let Some(saved) = self
            .allocation_forgetting_completion(&request.claim.intent)
            .await?
        {
            return identical(saved, request, o);
        }
        let p = &request.claim.intent.permit;
        let mut tx = self.pool().begin().await?;
        sqlx::query("SET LOCAL statement_timeout='5s'")
            .execute(&mut *tx)
            .await?;
        sqlx::query("SELECT id FROM hosts WHERE id=$1 AND supervisor_epoch=$2 AND launch_authority_required AND registered_allocation_serial>=$3 FOR SHARE")
            .bind(p.host.uuid()).bind(request.claim.reporting_epoch).bind(p.serial as i64).fetch_optional(&mut *tx).await?.ok_or(Error::LostClaim)?;
        let (a, issued, r) = rows(&mut tx, p.allocation, true).await?;
        if issued != *p {
            return Err(Error::Evidence);
        }
        let metadata = completion::retained(&r, p, &a)?.ok_or(Error::Evidence)?;
        if metadata.request != request.metadata_request {
            return Err(Error::Evidence);
        }
        if let Some(saved) = retained(&r, p, &a)? {
            tx.rollback().await?;
            return identical(saved, request, o);
        }
        let expires = r
            .try_get::<Option<OffsetDateTime>, _>("forget_lease_expires_at")?
            .ok_or(Error::LostClaim)?;
        if r.try_get::<i64, _>("forget_revision")? != request.claim.revision
            || r.try_get::<Option<i64>, _>("forget_epoch")? != Some(request.claim.reporting_epoch)
            || millis(expires)? != request.claim.expires_unix_ms
        {
            return Err(Error::LostClaim);
        }
        if frozen_scope(
            &mut tx,
            &a,
            issued,
            request.claim.intent.retirement,
            false,
            request.claim.reporting_epoch,
        )
        .await?
            != request.claim.intent
        {
            return Err(Error::Evidence);
        }
        let e = Evidence {
            version: 1,
            request: request.clone(),
            state: o.state,
            observed_unix_ms: o.observed_unix_ms,
        };
        let at:OffsetDateTime=sqlx::query_scalar("WITH decision AS MATERIALIZED (SELECT clock_timestamp() AS now) UPDATE allocation_retirements SET forget_completion=$5,forgotten_at=decision.now,forget_claim_expires_at=forget_lease_expires_at,forget_lease_expires_at=NULL FROM decision WHERE allocation_id=$1 AND forget_revision=$2 AND forget_epoch=$3 AND forget_lease_expires_at=$4 AND forget_lease_expires_at>decision.now AND forget_completion IS NULL AND $6::bigint<=floor(extract(epoch FROM forget_lease_expires_at)*1000)::bigint AND $6::bigint BETWEEN floor(extract(epoch FROM decision.now)*1000)::bigint-10000 AND floor(extract(epoch FROM decision.now)*1000)::bigint+5000 RETURNING forgotten_at")
            .bind(p.allocation.uuid()).bind(request.claim.revision).bind(request.claim.reporting_epoch).bind(expires).bind(serde_json::to_value(e).map_err(|_|Error::Evidence)?).bind(o.observed_unix_ms)
            .fetch_optional(&mut *tx).await?.ok_or(Error::LostClaim)?;
        tx.commit().await?;
        Ok(ForgettingCompletion {
            request: request.clone(),
            state: state(o.state)?,
            observed_unix_ms: o.observed_unix_ms,
            completed_at: at,
        })
    }
}
