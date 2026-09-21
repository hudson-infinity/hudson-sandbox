use super::*;
use crate::Store;
use sandbox_protocol::{files, supervisor::FileObservation};
use serde_json::{Value, json};
pub(super) async fn finish(
    db: &mut PgConnection,
    claim: &Claim,
    status: &str,
    phase: &str,
    error: Option<Value>,
    result: Option<Value>,
) -> Result<(), DispatchError> {
    let terminal = matches!(status, "succeeded" | "failed" | "cancelled");
    let changed=sqlx::query("UPDATE operations SET status=$3,phase=$4,error=$5,result=$6,completed_at=CASE WHEN $7 THEN clock_timestamp() ELSE NULL END,lease_expires_at=NULL,next_retry_at=CASE WHEN $7 THEN NULL ELSE clock_timestamp()+interval '100 milliseconds' END,updated_at=clock_timestamp() WHERE id=$1 AND claim_revision=$2 AND lease_expires_at>clock_timestamp() AND status IN ('running','unknown')")
 .bind(claim.operation_id.uuid()).bind(claim.revision).bind(status).bind(phase).bind(error).bind(result).bind(terminal).execute(db).await?.rows_affected();
    if changed != 1 {
        return Err(DispatchError::LostClaim);
    }
    Ok(())
}
impl Store {
    /// `written` is the exact expected acknowledgement of this original write call.
    /// Inspection never derives a new write cursor from an untrusted guest response.
    pub async fn observe_upload(
        &self,
        claim: &Claim,
        o: &FileObservation,
        written: Option<u64>,
        allow_simulated: bool,
    ) -> Result<(), DispatchError> {
        let mut tx = self.pool().begin().await?;
        let c = context(&mut tx, claim).await?;
        if !c.begun()?
            || o.ownership.as_ref() != Some(&c.owner)
            || o.upload_digest
                != c.plan
                    .upload
                    .digest()
                    .map_err(|_| DispatchError::InvalidData)?
            || o.stored != written
            || !(0..=5).contains(&o.state)
        {
            return Err(DispatchError::BadEvidence);
        }
        if o.simulated && !allow_simulated {
            return Err(DispatchError::SimulationDenied);
        }
        let fresh: bool = sqlx::query_scalar(
            "SELECT abs(extract(epoch FROM clock_timestamp())*1000-$1::bigint)<=10000",
        )
        .bind(o.observed_unix_ms)
        .fetch_one(&mut *tx)
        .await?;
        if !fresh {
            return Err(DispatchError::BadEvidence);
        }
        let mut record = if o.not_started {
            if o.context.is_some()
                || o.state != 0
                || o.stored.is_some()
                || c.record.as_ref().is_some_and(|r| !r.not_started)
            {
                return Err(DispatchError::BadEvidence);
            }
            FileRecord::fenced(
                c.plan
                    .upload
                    .digest()
                    .map_err(|_| DispatchError::InvalidData)?,
            )
        } else if let Some(ctx) = &o.context {
            let ctx: sandbox_protocol::guest_model::Context = ctx
                .clone()
                .try_into()
                .map_err(|_| DispatchError::BadEvidence)?;
            if ctx.allocation_id != c.plan.owner.scope.allocation_id
                || ctx.generation != c.plan.owner.scope.generation
            {
                return Err(DispatchError::BadEvidence);
            }
            let mut r = match &c.record {
                Some(r) => r.clone(),
                None => FileRecord::pending(&c.plan.upload, ctx.clone())
                    .map_err(|_| DispatchError::BadEvidence)?,
            };
            if r.not_started || r.context.as_ref() != Some(&ctx) {
                return Err(DispatchError::BadEvidence);
            }
            r.commit_requested = c.flag("commit_requested")?;
            r.abort_requested = c.flag("abort_requested")?;
            if o.state != 0 {
                r.observe(
                    &c.plan.upload,
                    &files::Receipt {
                        version: 1,
                        context: ctx,
                        upload: c.plan.upload.clone(),
                        digest: r.digest,
                        state: match o.state {
                            1 => files::State::Staging,
                            2 => files::State::CommitIntent,
                            3 => files::State::Committed,
                            4 => files::State::Unknown,
                            5 => files::State::Aborted,
                            _ => return Err(DispatchError::BadEvidence),
                        },
                    },
                )
                .map_err(|_| DispatchError::BadEvidence)?;
            }
            r
        } else {
            if o.state != 0 || o.stored.is_some() {
                return Err(DispatchError::BadEvidence);
            }
            drop(tx);
            return self.upload_unknown(claim).await;
        };
        // A no-observation reply preserves prior evidence but cannot replay a terminal result.
        let state = if o.state == 0 && !o.not_started {
            0
        } else {
            record.state
        };
        if let Some(next) = written {
            let old = c.file.try_get::<i64, _>("written")? as u64;
            if state != 1
                || c.flag("commit_requested")?
                || c.flag("abort_requested")?
                || next
                    != old
                        + (c.plan.upload.size - old)
                            .min(sandbox_protocol::files::MAX_CHUNK_BYTES as u64)
                || next <= old
            {
                return Err(DispatchError::BadEvidence);
            }
            sqlx::query("UPDATE file_uploads SET written=$2 WHERE operation_id=$1")
                .bind(claim.operation_id.uuid())
                .bind(next as i64)
                .execute(&mut *tx)
                .await?;
        }
        if !record.not_started {
            record.commit_requested = c.flag("commit_requested")?;
            record.abort_requested = c.flag("abort_requested")?;
        }
        record.validate().map_err(|_| DispatchError::BadEvidence)?;
        sqlx::query("UPDATE file_uploads SET record=$2,needs_inspect=$3 WHERE operation_id=$1")
            .bind(claim.operation_id.uuid())
            .bind(json!(record))
            .bind(state != 1)
            .execute(&mut *tx)
            .await?;
        let (status, phase) = if o.not_started {
            ("failed", "file_not_started")
        } else {
            match state {
                3 => ("succeeded", "file_committed"),
                5 => ("failed", "file_aborted"),
                1 => ("running", "file_staging"),
                _ => ("unknown", "reconciling"),
            }
        };
        let result = if state == 3 {
            Some(
                json!({"simulated":o.simulated,"guest_reported":true,"size":c.plan.upload.size,"sha256":hex::encode(c.plan.upload.sha256)}),
            )
        } else {
            None
        };
        let error = if matches!(status, "failed" | "unknown") {
            Some(json!({"code":phase,"simulated":o.simulated}))
        } else {
            None
        };
        // Do not accept a response whose evidence aged while waiting for a database lock.
        let fresh: bool = sqlx::query_scalar(
            "SELECT abs(extract(epoch FROM clock_timestamp())*1000-$1::bigint)<=10000",
        )
        .bind(o.observed_unix_ms)
        .fetch_one(&mut *tx)
        .await?;
        if !fresh {
            return Err(DispatchError::BadEvidence);
        }
        finish(&mut tx, claim, status, phase, error, result).await?;
        tx.commit().await?;
        Ok(())
    }
}
