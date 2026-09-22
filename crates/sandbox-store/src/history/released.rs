//! Independent destruction claims; released_at schedules verification, never proves it.
use super::*;
use sandbox_protocol::supervisor::{
    AllocationState, ReleasedHistoryObservation, ReleasedHistoryRequest,
};
use serde_json::Value;

#[derive(Debug, Clone)]
pub struct ReleasedClaim {
    pub claim: Claim,
    pub reporting_epoch: i64,
}
#[derive(Debug)]
pub struct ReleasedPreparation {
    pub claim: ReleasedClaim,
    pub request: ReleasedHistoryRequest,
}
fn request(a: &PgRow, h: &PgRow, c: &ReleasedClaim) -> Result<ReleasedHistoryRequest, Error> {
    let through = OperationId::from_uuid(h.try_get("reserved_through")?);
    sandbox_protocol::history::advance_operation_id(through, None).map_err(|_| Error::Evidence)?;
    Ok(ReleasedHistoryRequest {
        ownership: Some(coordinator::owner(a, &c.claim)?),
        reporting_epoch: c.reporting_epoch,
        domain: match c.claim.domain {
            Domain::Commands => 1,
            Domain::Files => 2,
        },
        through: through.to_string(),
    })
}
fn context(h: Option<&PgRow>) -> Result<Option<Context>, Error> {
    h.map(|h| h.try_get::<Option<Value>, _>("context"))
        .transpose()?
        .flatten()
        .map(serde_json::from_value)
        .transpose()
        .map_err(|_| Error::Evidence)
}
async fn floor(
    db: &mut PgConnection,
    id: uuid::Uuid,
    domain: Domain,
    h: Option<&PgRow>,
) -> Result<Option<uuid::Uuid>, Error> {
    if let Some(completed) = h
        .map(|h| h.try_get::<Option<uuid::Uuid>, _>("completed_through"))
        .transpose()?
        .flatten()
    {
        let verified=sqlx::query_scalar::<_,uuid::Uuid>("SELECT completed_through FROM completed_released_allocation_history WHERE allocation_id=$1 AND domain=$2")
            .bind(id).bind(name(domain)).fetch_optional(&mut *db).await?;
        if verified != Some(completed) {
            return Err(Error::Evidence);
        }
        return Ok(Some(completed));
    }
    let raw = sqlx::query_scalar::<_, Option<uuid::Uuid>>(
        "SELECT completed_through FROM allocation_history WHERE allocation_id=$1 AND domain=$2",
    )
    .bind(id)
    .bind(name(domain))
    .fetch_optional(&mut *db)
    .await?
    .flatten();
    let verified=sqlx::query_scalar::<_,uuid::Uuid>("SELECT completed_through FROM completed_live_allocation_history WHERE allocation_id=$1 AND domain=$2")
        .bind(id).bind(name(domain)).fetch_optional(&mut *db).await?;
    if raw != verified
        || h.is_some_and(|h| h.get::<Option<uuid::Uuid>, _>("start_after") != verified)
    {
        return Err(Error::Evidence);
    }
    Ok(verified)
}
async fn rows(
    db: &mut PgConnection,
    id: uuid::Uuid,
    domain: Domain,
    after: Option<uuid::Uuid>,
    through: Option<uuid::Uuid>,
) -> Result<(Vec<PgRow>, usize), Error> {
    let (query, max) = match domain {
        Domain::Commands => (
            "SELECT * FROM operations WHERE execution_allocation_id=$1 AND ($2::uuid IS NULL OR id>$2) AND ($3::uuid IS NULL OR id<=$3) ORDER BY id LIMIT 34",
            33,
        ),
        Domain::Files => (
            "SELECT * FROM operations WHERE file_allocation_id=$1 AND ($2::uuid IS NULL OR id>$2) AND ($3::uuid IS NULL OR id<=$3) ORDER BY id LIMIT 18",
            17,
        ),
    };
    Ok((
        sqlx::query(query)
            .bind(id)
            .bind(after)
            .bind(through)
            .fetch_all(db)
            .await?,
        max,
    ))
}
async fn prefix(
    db: &mut PgConnection,
    a: &PgRow,
    h: &PgRow,
    domain: Domain,
    allow_simulated: bool,
) -> Result<(), Error> {
    let id = a.try_get("id")?;
    let after = floor(db, id, domain, Some(h)).await?;
    let through = h.try_get("reserved_through")?;
    let (rows, max) = rows(db, id, domain, after, Some(through)).await?;
    if rows.is_empty()
        || rows.len() > max
        || rows
            .last()
            .ok_or(Error::Evidence)?
            .try_get::<uuid::Uuid, _>("id")?
            != through
    {
        return Err(Error::Evidence);
    }
    let frozen = context(Some(h))?;
    let mut found = frozen.clone();
    for row in rows {
        let next = evidence::eligible(db, &row, a, domain, allow_simulated)
            .await?
            .ok_or(Error::Evidence)?;
        if let Some(next) = next {
            if found.as_ref().is_some_and(|c| c != &next) {
                return Err(Error::Evidence);
            }
            found = Some(next);
        }
    }
    if found != frozen {
        return Err(Error::Evidence);
    }
    Ok(())
}
async fn lock(db: &mut PgConnection, c: &ReleasedClaim) -> Result<(PgRow, PgRow), Error> {
    let a = sqlx::query("SELECT * FROM allocations WHERE id=$1 FOR UPDATE")
        .bind(c.claim.allocation_id.uuid())
        .fetch_one(&mut *db)
        .await?;
    let h=sqlx::query("SELECT * FROM released_allocation_history h WHERE allocation_id=$1 AND domain=$2 AND claim_revision=$3 AND reporting_epoch=$4 AND lease_expires_at=$5 AND lease_expires_at>clock_timestamp() AND EXISTS(SELECT 1 FROM allocations a JOIN hosts host ON host.id=a.host_id WHERE a.id=h.allocation_id AND a.status='released' AND a.released_at IS NOT NULL AND a.supervisor_epoch<=h.reporting_epoch AND host.supervisor_epoch=h.reporting_epoch)")
        .bind(c.claim.allocation_id.uuid()).bind(name(c.claim.domain)).bind(c.claim.revision).bind(c.reporting_epoch).bind(c.claim.expires_at)
        .fetch_optional(&mut *db).await?.ok_or(Error::LostClaim)?;
    Ok((a, h))
}
async fn candidate(
    db: &mut PgConnection,
    a: &PgRow,
    domain: Domain,
    epoch: i64,
    seconds: u32,
    allow_simulated: bool,
) -> Result<Option<ReleasedPreparation>, Error> {
    let id: uuid::Uuid = a.try_get("id")?;
    let old = sqlx::query(
        "SELECT * FROM released_allocation_history WHERE allocation_id=$1 AND domain=$2",
    )
    .bind(id)
    .bind(name(domain))
    .fetch_optional(&mut *db)
    .await?;
    let after = floor(db, id, domain, old.as_ref()).await?;
    let pending = old.as_ref().is_some_and(|h| {
        h.get::<Option<uuid::Uuid>, _>("completed_through") != Some(h.get("reserved_through"))
    });
    if !pending {
        let (rows, max) = rows(db, id, domain, after, None).await?;
        let mut through = None;
        let mut frozen = context(old.as_ref())?;
        for row in rows.into_iter().take(max) {
            let Some(next) = evidence::eligible(db, &row, a, domain, allow_simulated).await? else {
                break;
            };
            if let Some(next) = next {
                if frozen.as_ref().is_some_and(|c| c != &next) {
                    return Err(Error::Evidence);
                }
                frozen = Some(next);
            }
            through = Some(row.try_get::<uuid::Uuid, _>("id")?);
        }
        let Some(through) = through else {
            return Ok(None);
        };
        sqlx::query("INSERT INTO released_allocation_history(allocation_id,domain,start_after,reserved_through,context,reporting_epoch) VALUES($1,$2,$3,$4,$5,$6) ON CONFLICT(allocation_id,domain) DO UPDATE SET reserved_through=$4,context=$5,request=NULL")
            .bind(id).bind(name(domain)).bind(after).bind(through).bind(frozen.map(serde_json::to_value).transpose().map_err(|_|Error::Evidence)?).bind(epoch).execute(&mut *db).await?;
    }
    let h=sqlx::query("UPDATE released_allocation_history SET claim_revision=claim_revision+1,reporting_epoch=$3,lease_expires_at=clock_timestamp()+make_interval(secs=>$4),next_retry_at=NULL,request=NULL WHERE allocation_id=$1 AND domain=$2 RETURNING *")
        .bind(id).bind(name(domain)).bind(epoch).bind(f64::from(seconds)).fetch_one(&mut *db).await?;
    let c = ReleasedClaim {
        claim: Claim {
            allocation_id: AllocationId::from_uuid(id),
            domain,
            revision: h.try_get("claim_revision")?,
            expires_at: h.try_get("lease_expires_at")?,
        },
        reporting_epoch: epoch,
    };
    prefix(db, a, &h, domain, allow_simulated).await?;
    let r = request(a, &h, &c)?;
    let n=sqlx::query("UPDATE released_allocation_history h SET request=$4 WHERE allocation_id=$1 AND domain=$2 AND claim_revision=$3 AND lease_expires_at>clock_timestamp() AND EXISTS(SELECT 1 FROM allocations a JOIN hosts host ON host.id=a.host_id WHERE a.id=h.allocation_id AND a.status='released' AND a.released_at IS NOT NULL AND a.supervisor_epoch<=h.reporting_epoch AND host.supervisor_epoch=h.reporting_epoch)")
        .bind(id).bind(name(domain)).bind(c.claim.revision).bind(serde_json::to_value(&r).map_err(|_|Error::Evidence)?).execute(db).await?.rows_affected();
    if n != 1 {
        return Err(Error::LostClaim);
    }
    Ok(Some(ReleasedPreparation {
        claim: c,
        request: r,
    }))
}
impl Store {
    pub async fn claim_released_history(
        &self,
        host: HostId,
        epoch: i64,
        domain: Domain,
        seconds: u32,
        allow_simulated: bool,
    ) -> Result<Option<ReleasedPreparation>, Error> {
        if epoch <= 0 || !(1..=300).contains(&seconds) {
            return Err(Error::Policy);
        }
        let mut tx = self.pool().begin().await?;
        sqlx::query("SET LOCAL statement_timeout='5s'")
            .execute(&mut *tx)
            .await?;
        let candidates=sqlx::query("SELECT a.* FROM allocations a JOIN hosts host ON host.id=a.host_id LEFT JOIN released_allocation_history h ON h.allocation_id=a.id AND h.domain=$3 WHERE a.host_id=$1 AND a.supervisor_epoch<=$2 AND host.supervisor_epoch=$2 AND a.status='released' AND a.released_at IS NOT NULL AND (h.lease_expires_at IS NULL OR h.lease_expires_at<=clock_timestamp()) AND (h.next_retry_at IS NULL OR h.next_retry_at<=clock_timestamp()) ORDER BY CASE WHEN $3='commands' THEN a.history_commands_scan_at ELSE a.history_files_scan_at END NULLS FIRST,a.id LIMIT 32 FOR UPDATE OF a SKIP LOCKED")
            .bind(host.uuid()).bind(epoch).bind(name(domain)).fetch_all(&mut *tx).await?;
        for a in candidates {
            let id: uuid::Uuid = a.try_get("id")?;
            sqlx::query("UPDATE allocations SET history_commands_scan_at=CASE WHEN $2='commands' THEN clock_timestamp() ELSE history_commands_scan_at END,history_files_scan_at=CASE WHEN $2='files' THEN clock_timestamp() ELSE history_files_scan_at END WHERE id=$1")
                .bind(id).bind(name(domain)).execute(&mut *tx).await?;
            sqlx::query("SAVEPOINT released_candidate")
                .execute(&mut *tx)
                .await?;
            match candidate(&mut tx, &a, domain, epoch, seconds, allow_simulated).await {
                Ok(Some(p)) => {
                    tx.commit().await?;
                    return Ok(Some(p));
                }
                Ok(None) => {
                    sqlx::query("RELEASE SAVEPOINT released_candidate")
                        .execute(&mut *tx)
                        .await?;
                }
                Err(Error::Evidence) => {
                    sqlx::query("ROLLBACK TO SAVEPOINT released_candidate")
                        .execute(&mut *tx)
                        .await?;
                    sqlx::query("RELEASE SAVEPOINT released_candidate")
                        .execute(&mut *tx)
                        .await?;
                    sqlx::query("UPDATE released_allocation_history SET next_retry_at=clock_timestamp()+interval '5 seconds' WHERE allocation_id=$1 AND domain=$2")
                        .bind(id).bind(name(domain)).execute(&mut *tx).await?;
                }
                Err(e) => return Err(e),
            }
        }
        tx.commit().await?;
        Ok(None)
    }
    pub async fn complete_released_history(
        &self,
        c: &ReleasedClaim,
        observed: &ReleasedHistoryObservation,
        allow_simulated: bool,
    ) -> Result<(), Error> {
        let mut tx = self.pool().begin().await?;
        sqlx::query("SET LOCAL statement_timeout='5s'")
            .execute(&mut *tx)
            .await?;
        let (a, h) = lock(&mut tx, c).await?;
        prefix(&mut tx, &a, &h, c.claim.domain, allow_simulated).await?;
        let expected = request(&a, &h, c)?;
        let stored: ReleasedHistoryRequest =
            serde_json::from_value(h.try_get("request")?).map_err(|_| Error::Evidence)?;
        if stored != expected
            || observed.request.as_ref() != Some(&expected)
            || observed.completed_through != expected.through
            || !matches!(observed.release_state, x if x==AllocationState::Released as i32 || x==AllocationState::FencedAbsent as i32)
            || observed.simulated && !allow_simulated
        {
            return Err(Error::Evidence);
        }
        let now: OffsetDateTime = sqlx::query_scalar("SELECT clock_timestamp()")
            .fetch_one(&mut *tx)
            .await?;
        coordinator::fresh(observed.observed_unix_ms, &c.claim, now)?;
        let n=sqlx::query("UPDATE released_allocation_history h SET completed_through=reserved_through,completion=$5,completed_at=clock_timestamp(),lease_expires_at=NULL,next_retry_at=NULL WHERE allocation_id=$1 AND domain=$2 AND claim_revision=$3 AND reporting_epoch=$4 AND lease_expires_at>clock_timestamp() AND EXISTS(SELECT 1 FROM allocations a JOIN hosts host ON host.id=a.host_id WHERE a.id=h.allocation_id AND a.status='released' AND a.released_at IS NOT NULL AND a.supervisor_epoch<=h.reporting_epoch AND host.supervisor_epoch=h.reporting_epoch)")
            .bind(c.claim.allocation_id.uuid()).bind(name(c.claim.domain)).bind(c.claim.revision).bind(c.reporting_epoch).bind(serde_json::to_value(observed).map_err(|_|Error::Evidence)?)
            .execute(&mut *tx).await?.rows_affected();
        if n != 1 {
            return Err(Error::LostClaim);
        }
        tx.commit().await?;
        Ok(())
    }
    pub async fn defer_released_history(&self, c: &ReleasedClaim) -> Result<(), Error> {
        let mut tx = self.pool().begin().await?;
        sqlx::query("SET LOCAL statement_timeout='5s'")
            .execute(&mut *tx)
            .await?;
        lock(&mut tx, c).await?;
        let n=sqlx::query("UPDATE released_allocation_history h SET lease_expires_at=NULL,next_retry_at=clock_timestamp()+interval '5 seconds' WHERE allocation_id=$1 AND domain=$2 AND claim_revision=$3 AND reporting_epoch=$4 AND lease_expires_at>clock_timestamp() AND EXISTS(SELECT 1 FROM allocations a JOIN hosts host ON host.id=a.host_id WHERE a.id=h.allocation_id AND a.status='released' AND a.released_at IS NOT NULL AND a.supervisor_epoch<=h.reporting_epoch AND host.supervisor_epoch=h.reporting_epoch)")
            .bind(c.claim.allocation_id.uuid()).bind(name(c.claim.domain)).bind(c.claim.revision).bind(c.reporting_epoch).execute(&mut *tx).await?.rows_affected();
        if n != 1 {
            return Err(Error::LostClaim);
        }
        tx.commit().await?;
        Ok(())
    }
}
