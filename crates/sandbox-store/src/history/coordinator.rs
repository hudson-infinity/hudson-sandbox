use super::*;

#[derive(Debug)]
pub enum Preparation {
    Binding {
        claim: Claim,
        request: LeaseInspection,
    },
    Retire {
        claim: Claim,
        request: HistoryRequest,
    },
}

pub(super) fn owner(a: &PgRow, claim: &Claim) -> Result<LeaseOwnership, Error> {
    Ok(LeaseOwnership {
        host_id: HostId::from_uuid(a.try_get("host_id")?).to_string(),
        project_id: ProjectId::from_uuid(a.try_get("project_id")?).to_string(),
        sandbox_id: SandboxId::from_uuid(a.try_get("sandbox_id")?).to_string(),
        allocation_id: claim.allocation_id.to_string(),
        generation: a.try_get("generation")?,
        supervisor_epoch: a.try_get("supervisor_epoch")?,
        revision: claim.revision,
        claim_expires_unix_ms: millis(claim.expires_at)?,
    })
}
async fn lock(db: &mut PgConnection, claim: &Claim) -> Result<(PgRow, PgRow), Error> {
    let a = sqlx::query("SELECT * FROM allocations WHERE id=$1 FOR UPDATE")
        .bind(claim.allocation_id.uuid())
        .fetch_one(&mut *db)
        .await?;
    let live:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM allocations a JOIN hosts h ON h.id=a.host_id WHERE a.id=$1 AND a.status='running' AND a.released_at IS NULL AND a.lease_expires_at>clock_timestamp() AND a.supervisor_epoch=h.supervisor_epoch)")
        .bind(claim.allocation_id.uuid()).fetch_one(&mut *db).await?;
    if !live {
        return Err(Error::LostClaim);
    }
    let h=sqlx::query("SELECT * FROM allocation_history WHERE allocation_id=$1 AND domain=$2 AND claim_revision=$3 AND lease_expires_at=$4 AND lease_expires_at>clock_timestamp() AND EXISTS(SELECT 1 FROM allocations a JOIN hosts host ON host.id=a.host_id WHERE a.id=allocation_history.allocation_id AND a.status='running' AND a.released_at IS NULL AND a.lease_expires_at>clock_timestamp() AND a.supervisor_epoch=host.supervisor_epoch)")
        .bind(claim.allocation_id.uuid()).bind(name(claim.domain)).bind(claim.revision).bind(claim.expires_at)
        .fetch_optional(&mut *db).await?.ok_or(Error::LostClaim)?;
    Ok((a, h))
}
fn request(a: &PgRow, h: &PgRow, claim: &Claim, context: Context) -> Result<HistoryRequest, Error> {
    if context.allocation_id != claim.allocation_id
        || context.generation != a.try_get::<i64, _>("generation")?
    {
        return Err(Error::Evidence);
    }
    let barrier = Barrier {
        version: 1,
        context,
        domain: claim.domain,
        through: OperationId::from_uuid(h.try_get("reserved_through")?),
    };
    barrier
        .validate(&barrier.context, claim.domain)
        .map_err(|_| Error::Evidence)?;
    Ok(HistoryRequest {
        ownership: Some(owner(a, claim)?),
        barrier: Some((&barrier).into()),
    })
}
async fn save_request(
    db: &mut PgConnection,
    claim: &Claim,
    r: &HistoryRequest,
    context: &Context,
) -> Result<(), Error> {
    let changed=sqlx::query("UPDATE allocation_history SET context=$4,request=$5 WHERE allocation_id=$1 AND domain=$2 AND claim_revision=$3 AND lease_expires_at>clock_timestamp() AND EXISTS(SELECT 1 FROM allocations a JOIN hosts host ON host.id=a.host_id WHERE a.id=allocation_history.allocation_id AND a.status='running' AND a.released_at IS NULL AND a.lease_expires_at>clock_timestamp() AND a.supervisor_epoch=host.supervisor_epoch)")
        .bind(claim.allocation_id.uuid()).bind(name(claim.domain)).bind(claim.revision)
        .bind(serde_json::to_value(context).map_err(|_|Error::Evidence)?)
        .bind(serde_json::to_value(r).map_err(|_|Error::Evidence)?).execute(db).await?;
    if changed.rows_affected() != 1 {
        return Err(Error::LostClaim);
    }
    Ok(())
}
pub(super) fn fresh(observed: i64, claim: &Claim, now: OffsetDateTime) -> Result<(), Error> {
    // Observations must belong to this short claim, allowing bounded clock skew.
    if observed <= 0
        || observed > millis(now)?.saturating_add(5_000)
        || observed > millis(claim.expires_at)?
        || observed < millis(now)?.saturating_sub(10_000)
    {
        return Err(Error::Evidence);
    }
    Ok(())
}
impl Store {
    /// Trusted controller entry point. Reserves a contiguous prefix before any RPC.
    /// Pending reservations are reclaimed using the same prefix, never enlarged.
    pub async fn claim_history(
        &self,
        host: HostId,
        epoch: i64,
        domain: Domain,
        seconds: u32,
        allow_simulated: bool,
    ) -> Result<Option<Preparation>, Error> {
        if !(1..=300).contains(&seconds) || epoch <= 0 {
            return Err(Error::Policy);
        }
        let mut tx = self.pool().begin().await?;
        sqlx::query("SET LOCAL statement_timeout='5s'")
            .execute(&mut *tx)
            .await?;
        // No project, sandbox, host or operation locks may follow this lock.
        let candidates=sqlx::query("SELECT a.* FROM allocations a JOIN hosts host ON host.id=a.host_id LEFT JOIN allocation_history h ON h.allocation_id=a.id AND h.domain=$3 WHERE a.host_id=$1 AND a.supervisor_epoch=$2 AND host.supervisor_epoch=$2 AND a.status='running' AND a.lease_expires_at>clock_timestamp() AND (h.lease_expires_at IS NULL OR h.lease_expires_at<=clock_timestamp()) AND (h.next_retry_at IS NULL OR h.next_retry_at<=clock_timestamp()) ORDER BY CASE WHEN $3='commands' THEN a.history_commands_scan_at ELSE a.history_files_scan_at END NULLS FIRST,a.id LIMIT 32 FOR UPDATE OF a SKIP LOCKED")
            .bind(host.uuid()).bind(epoch).bind(name(domain)).fetch_all(&mut *tx).await?;
        for a in candidates {
            let id: uuid::Uuid = a.try_get("id")?;
            sqlx::query("UPDATE allocations SET history_commands_scan_at=CASE WHEN $2='commands' THEN clock_timestamp() ELSE history_commands_scan_at END,history_files_scan_at=CASE WHEN $2='files' THEN clock_timestamp() ELSE history_files_scan_at END WHERE id=$1").bind(id).bind(name(domain)).execute(&mut *tx).await?;
            sqlx::query("SAVEPOINT history_candidate")
                .execute(&mut *tx)
                .await?;
            match prepare_candidate(&mut tx, &a, domain, seconds, allow_simulated).await {
                Ok(Some(result)) => {
                    tx.commit().await?;
                    return Ok(Some(result));
                }
                Ok(None) => {
                    sqlx::query("RELEASE SAVEPOINT history_candidate")
                        .execute(&mut *tx)
                        .await?;
                }
                Err(Error::Evidence) => {
                    sqlx::query("ROLLBACK TO SAVEPOINT history_candidate")
                        .execute(&mut *tx)
                        .await?;
                    sqlx::query("RELEASE SAVEPOINT history_candidate")
                        .execute(&mut *tx)
                        .await?;
                    sqlx::query("UPDATE allocation_history SET next_retry_at=clock_timestamp()+interval '5 seconds' WHERE allocation_id=$1 AND domain=$2").bind(id).bind(name(domain)).execute(&mut *tx).await?;
                }
                Err(error) => return Err(error),
            }
        }
        tx.commit().await?;
        Ok(None)
    }
    pub async fn bind_history(
        &self,
        claim: &Claim,
        observation: &HistoryBindingObservation,
        allow_simulated: bool,
    ) -> Result<HistoryRequest, Error> {
        let mut tx = self.pool().begin().await?;
        sqlx::query("SET LOCAL statement_timeout='5s'")
            .execute(&mut *tx)
            .await?;
        let (a, h) = lock(&mut tx, claim).await?;
        let now: OffsetDateTime = sqlx::query_scalar("SELECT clock_timestamp()")
            .fetch_one(&mut *tx)
            .await?;
        if observation.simulated && !allow_simulated
            || observation.request
                != Some(LeaseInspection {
                    ownership: Some(owner(&a, claim)?),
                })
        {
            return Err(Error::Evidence);
        }
        fresh(observation.observed_unix_ms, claim, now)?;
        let context: Context = observation
            .context
            .clone()
            .ok_or(Error::Evidence)?
            .try_into()
            .map_err(|_| Error::Evidence)?;
        if let Some(saved) = h.try_get::<Option<serde_json::Value>, _>("context")?
            && serde_json::from_value::<Context>(saved).map_err(|_| Error::Evidence)? != context
        {
            return Err(Error::Evidence);
        }
        validate_prefix(&mut tx, &a, &h, claim, &context, allow_simulated).await?;
        let r = request(&a, &h, claim, context.clone())?;
        save_request(&mut tx, claim, &r, &context).await?;
        tx.commit().await?;
        Ok(r)
    }
    /// Only this exact acknowledgement promotes the reserved prefix to completed.
    pub async fn complete_history(
        &self,
        claim: &Claim,
        observation: &HistoryObservation,
        allow_simulated: bool,
    ) -> Result<(), Error> {
        let mut tx = self.pool().begin().await?;
        sqlx::query("SET LOCAL statement_timeout='5s'")
            .execute(&mut *tx)
            .await?;
        let (a, h) = lock(&mut tx, claim).await?;
        let context: Context =
            serde_json::from_value(h.try_get("context")?).map_err(|_| Error::Evidence)?;
        validate_prefix(&mut tx, &a, &h, claim, &context, allow_simulated).await?;
        let expected = request(&a, &h, claim, context)?;
        let stored: HistoryRequest =
            serde_json::from_value(h.try_get("request")?).map_err(|_| Error::Evidence)?;
        if stored != expected
            || observation.request.as_ref() != Some(&expected)
            || observation.simulated && !allow_simulated
            || observation.completed != expected.barrier
        {
            return Err(Error::Evidence);
        }
        let now: OffsetDateTime = sqlx::query_scalar("SELECT clock_timestamp()")
            .fetch_one(&mut *tx)
            .await?;
        fresh(observation.observed_unix_ms, claim, now)?;
        let updated=sqlx::query("UPDATE allocation_history SET completed_through=reserved_through,completion=$4,completed_at=clock_timestamp(),lease_expires_at=NULL,next_retry_at=NULL WHERE allocation_id=$1 AND domain=$2 AND claim_revision=$3 AND lease_expires_at>clock_timestamp() AND EXISTS(SELECT 1 FROM allocations a JOIN hosts host ON host.id=a.host_id WHERE a.id=allocation_history.allocation_id AND a.status='running' AND a.released_at IS NULL AND a.lease_expires_at>clock_timestamp() AND a.supervisor_epoch=host.supervisor_epoch)")
            .bind(claim.allocation_id.uuid()).bind(name(claim.domain)).bind(claim.revision)
            .bind(serde_json::to_value(observation).map_err(|_|Error::Evidence)?).execute(&mut *tx).await?;
        if updated.rows_affected() != 1 {
            return Err(Error::LostClaim);
        }
        tx.commit().await?;
        Ok(())
    }
    pub async fn defer_history(&self, claim: &Claim) -> Result<(), Error> {
        let mut tx = self.pool().begin().await?;
        sqlx::query("SET LOCAL statement_timeout='5s'")
            .execute(&mut *tx)
            .await?;
        lock(&mut tx, claim).await?;
        let updated=sqlx::query("UPDATE allocation_history SET lease_expires_at=NULL,next_retry_at=clock_timestamp()+interval '5 seconds' WHERE allocation_id=$1 AND domain=$2 AND claim_revision=$3 AND lease_expires_at>clock_timestamp() AND EXISTS(SELECT 1 FROM allocations a JOIN hosts host ON host.id=a.host_id WHERE a.id=allocation_history.allocation_id AND a.status='running' AND a.released_at IS NULL AND a.lease_expires_at>clock_timestamp() AND a.supervisor_epoch=host.supervisor_epoch)")
            .bind(claim.allocation_id.uuid()).bind(name(claim.domain)).bind(claim.revision).execute(&mut *tx).await?;
        if updated.rows_affected() != 1 {
            return Err(Error::LostClaim);
        }
        tx.commit().await?;
        Ok(())
    }
}

async fn validate_prefix(
    db: &mut PgConnection,
    a: &PgRow,
    h: &PgRow,
    claim: &Claim,
    context: &Context,
    allow_simulated: bool,
) -> Result<(), Error> {
    let (query, max) = match claim.domain {
        Domain::Commands => (
            "SELECT * FROM operations WHERE execution_allocation_id=$1 AND ($2::uuid IS NULL OR id>$2) AND id<=$3 ORDER BY id LIMIT 34",
            33,
        ),
        Domain::Files => (
            "SELECT * FROM operations WHERE file_allocation_id=$1 AND ($2::uuid IS NULL OR id>$2) AND id<=$3 ORDER BY id LIMIT 18",
            17,
        ),
    };
    let through: uuid::Uuid = h.try_get("reserved_through")?;
    let rows = sqlx::query(query)
        .bind(claim.allocation_id.uuid())
        .bind(h.try_get::<Option<uuid::Uuid>, _>("completed_through")?)
        .bind(through)
        .fetch_all(&mut *db)
        .await?;
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
    for row in rows {
        let found = evidence::eligible(db, &row, a, claim.domain, allow_simulated)
            .await?
            .ok_or(Error::Evidence)?;
        if found.as_ref().is_some_and(|c| c != context) {
            return Err(Error::Evidence);
        }
    }
    Ok(())
}

async fn prepare_candidate(
    db: &mut PgConnection,
    a: &PgRow,
    domain: Domain,
    seconds: u32,
    allow_simulated: bool,
) -> Result<Option<Preparation>, Error> {
    let id: uuid::Uuid = a.try_get("id")?;
    let old = sqlx::query("SELECT * FROM allocation_history WHERE allocation_id=$1 AND domain=$2")
        .bind(id)
        .bind(name(domain))
        .fetch_optional(&mut *db)
        .await?;
    if let Some(h) = &old {
        let retained = h.try_get::<Option<uuid::Uuid>, _>("completed_through")?;
        if retained.is_some() {
            let verified=sqlx::query_scalar::<_,uuid::Uuid>("SELECT completed_through FROM completed_allocation_history WHERE allocation_id=$1 AND domain=$2").bind(id).bind(name(domain)).fetch_optional(&mut *db).await?;
            if verified != retained {
                return Err(Error::Evidence);
            }
        }
    }
    let pending = old.as_ref().is_some_and(|h| {
        h.get::<Option<uuid::Uuid>, _>("completed_through") != Some(h.get("reserved_through"))
    });
    if !pending {
        let completed = old
            .as_ref()
            .map(|h| h.try_get::<Option<uuid::Uuid>, _>("completed_through"))
            .transpose()?
            .flatten();
        let query = match domain {
            Domain::Commands => {
                "SELECT * FROM operations WHERE execution_allocation_id=$1 AND ($2::uuid IS NULL OR id>$2) ORDER BY id LIMIT 33"
            }
            Domain::Files => {
                "SELECT * FROM operations WHERE file_allocation_id=$1 AND ($2::uuid IS NULL OR id>$2) ORDER BY id LIMIT 17"
            }
        };
        let rows = sqlx::query(query)
            .bind(id)
            .bind(completed)
            .fetch_all(&mut *db)
            .await?;
        let mut through = None;
        let mut context: Option<Context> = old
            .as_ref()
            .map(|h| h.try_get::<Option<serde_json::Value>, _>("context"))
            .transpose()?
            .flatten()
            .map(serde_json::from_value)
            .transpose()
            .map_err(|_| Error::Evidence)?;
        for row in rows {
            let Some(found) =
                evidence::eligible(&mut *db, &row, a, domain, allow_simulated).await?
            else {
                break;
            };
            if let Some(found) = found {
                if context.as_ref().is_some_and(|c| c != &found) {
                    return Err(Error::Evidence);
                }
                context = Some(found);
            }
            through = Some(row.try_get::<uuid::Uuid, _>("id")?);
        }
        let Some(through) = through else {
            return Ok(None);
        };
        sqlx::query("INSERT INTO allocation_history(allocation_id,domain,reserved_through,context) VALUES($1,$2,$3,$4) ON CONFLICT(allocation_id,domain) DO UPDATE SET reserved_through=$3,context=$4,request=NULL")
                    .bind(id).bind(name(domain)).bind(through).bind(context.map(serde_json::to_value).transpose().map_err(|_|Error::Evidence)?)
                    .execute(&mut *db).await?;
    }
    let h=sqlx::query("UPDATE allocation_history SET claim_revision=claim_revision+1,lease_expires_at=clock_timestamp()+make_interval(secs=>$3),next_retry_at=NULL,request=NULL WHERE allocation_id=$1 AND domain=$2 RETURNING *")
                .bind(id).bind(name(domain)).bind(f64::from(seconds)).fetch_one(&mut *db).await?;
    let claim = Claim {
        allocation_id: AllocationId::from_uuid(id),
        domain,
        revision: h.try_get("claim_revision")?,
        expires_at: h.try_get("lease_expires_at")?,
    };
    let context: Option<serde_json::Value> = h.try_get("context")?;
    let result = if let Some(context) = context {
        let context: Context = serde_json::from_value(context).map_err(|_| Error::Evidence)?;
        validate_prefix(db, a, &h, &claim, &context, allow_simulated).await?;
        let request = request(a, &h, &claim, context.clone())?;
        save_request(&mut *db, &claim, &request, &context).await?;
        Preparation::Retire { claim, request }
    } else {
        let request = LeaseInspection {
            ownership: Some(owner(a, &claim)?),
        };
        Preparation::Binding { claim, request }
    };

    Ok(Some(result))
}
