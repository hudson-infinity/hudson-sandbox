use super::*;
use crate::Store;
use sandbox_protocol::{HostId, files::MAX_CHUNK_BYTES};
use serde_json::json;
#[derive(Debug)]
pub enum UploadAction {
    Source(SourcePlan),
    Begin(FileRequest),
    Write {
        request: FileRequest,
        source: Box<SourceRef>,
        offset: u64,
        limit: usize,
    },
    Commit(FileRequest),
    Abort(FileRequest),
    Inspect(FileRequest),
    Rejected,
}
impl Store {
    pub async fn prepare_upload(
        &self,
        claim: &Claim,
        host: HostId,
        epoch: i64,
    ) -> Result<UploadAction, DispatchError> {
        let mut tx = self.pool().begin().await?;
        let c = context(&mut tx, claim).await?;
        if c.plan.owner.scope.host_id != host || c.plan.owner.scope.host_epoch != epoch {
            return Err(DispatchError::HostUnavailable);
        }
        let allowed = eligible(&mut tx, c.plan.upload.operation_id).await?;
        if !c.begun()? {
            let now: OffsetDateTime = sqlx::query_scalar("SELECT clock_timestamp()")
                .fetch_one(&mut *tx)
                .await?;
            if !allowed || (c.source.is_none() && ms(now)? >= c.plan.write_expires_unix_ms) {
                super::observe::finish(
                    &mut tx,
                    claim,
                    "failed",
                    "file_not_started",
                    Some(json!({"code":"file_not_started"})),
                    None,
                )
                .await?;
                tx.commit().await?;
                return Ok(UploadAction::Rejected);
            }
            if c.source.is_none() {
                crate::dispatch::fence(&mut tx, claim).await?;
                tx.commit().await?;
                return Ok(UploadAction::Source(c.plan));
            }
            sqlx::query("UPDATE file_uploads SET begin_requested=true,needs_inspect=true WHERE operation_id=$1").bind(claim.operation_id.uuid()).execute(&mut *tx).await?;
            sqlx::query("UPDATE operations SET phase='file_begin_intent',attempt_count=1,attempt_receipts=$2 WHERE id=$1")
   .bind(claim.operation_id.uuid()).bind(json!([{"phase":"file_begin_intent","plan_sha256":c.plan.metadata_digest().map_err(|_|DispatchError::InvalidData)?}])).execute(&mut *tx).await?;
            mutation_fence(&mut tx, claim).await?;
            tx.commit().await?;
            return Ok(UploadAction::Begin(c.request()));
        }
        let request = c.request();
        let action = if !allowed && !c.flag("abort_requested")? {
            sqlx::query("UPDATE file_uploads SET abort_requested=true,needs_inspect=true WHERE operation_id=$1").bind(claim.operation_id.uuid()).execute(&mut *tx).await?;
            UploadAction::Abort(request)
        } else if c.flag("needs_inspect")?
            || c.flag("commit_requested")?
            || c.flag("abort_requested")?
        {
            UploadAction::Inspect(request)
        } else if c
            .record
            .as_ref()
            .is_some_and(|r| !r.not_started && r.state == 1)
        {
            let offset = c.file.try_get::<i64, _>("written")? as u64;
            if offset < c.plan.upload.size {
                UploadAction::Write {
                    request,
                    source: Box::new(c.source.clone().ok_or(DispatchError::InvalidData)?),
                    offset,
                    limit: MAX_CHUNK_BYTES.min((c.plan.upload.size - offset) as usize),
                }
            } else {
                sqlx::query("UPDATE file_uploads SET commit_requested=true,needs_inspect=true WHERE operation_id=$1").bind(claim.operation_id.uuid()).execute(&mut *tx).await?;
                UploadAction::Commit(request)
            }
        } else {
            UploadAction::Inspect(request)
        };
        // Recheck authority after intent updates, before committing a new mutation.
        if matches!(
            &action,
            UploadAction::Write { .. } | UploadAction::Commit(_)
        ) {
            mutation_fence(&mut tx, claim).await?;
        } else {
            crate::dispatch::fence(&mut tx, claim).await?;
        }
        tx.commit().await?;
        Ok(action)
    }
    pub async fn accept_upload_source(
        &self,
        claim: &Claim,
        source: &SourceRef,
    ) -> Result<(), DispatchError> {
        let mut tx = self.pool().begin().await?;
        let c = context(&mut tx, claim).await?;
        source.validate().map_err(|_| DispatchError::BadEvidence)?;
        if source.plan != c.plan || c.begun()? || c.source.as_ref().is_some_and(|r| r != source) {
            return Err(DispatchError::BadEvidence);
        }
        let now: OffsetDateTime = sqlx::query_scalar("SELECT clock_timestamp()")
            .fetch_one(&mut *tx)
            .await?;
        if ms(now)? >= c.plan.expires_unix_ms {
            return Err(DispatchError::Conflict);
        }
        sqlx::query("UPDATE file_uploads SET source_ref=$2 WHERE operation_id=$1")
            .bind(claim.operation_id.uuid())
            .bind(json!(source))
            .execute(&mut *tx)
            .await?;
        super::observe::finish(&mut tx, claim, "running", "source_ready", None, None).await?;
        tx.commit().await?;
        Ok(())
    }
    pub async fn upload_unknown(&self, claim: &Claim) -> Result<(), DispatchError> {
        let mut tx = self.pool().begin().await?;
        let c = context(&mut tx, claim).await?;
        if c.begun()? {
            sqlx::query("UPDATE file_uploads SET needs_inspect=true WHERE operation_id=$1")
                .bind(claim.operation_id.uuid())
                .execute(&mut *tx)
                .await?;
        }
        super::observe::finish(
            &mut tx,
            claim,
            if c.begun()? { "unknown" } else { "running" },
            if c.begun()? {
                "reconciling"
            } else {
                "awaiting_source"
            },
            Some(json!({"code":if c.begun()?{"outcome_unknown"}else{"source_unavailable"}})),
            None,
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }
}
impl Store {
    /// Recheck after slow source retrieval and immediately before sending a chunk.
    pub async fn confirm_upload_write(
        &self,
        claim: &Claim,
        request: &FileRequest,
        source: &SourceRef,
        offset: u64,
    ) -> Result<(), DispatchError> {
        let mut tx = self.pool().begin().await?;
        let c = context(&mut tx, claim).await?;
        if c.request() != *request
            || c.source.as_ref() != Some(source)
            || c.file.try_get::<i64, _>("written")? as u64 != offset
            || !c.begun()?
            || c.flag("needs_inspect")?
            || c.flag("commit_requested")?
            || c.flag("abort_requested")?
            || !c
                .record
                .as_ref()
                .is_some_and(|r| r.state == 1 && !r.not_started)
        {
            return Err(DispatchError::Conflict);
        }
        if !eligible(&mut tx, c.plan.upload.operation_id).await? {
            return Err(DispatchError::Unauthorized);
        }
        crate::dispatch::fence(&mut tx, claim).await?;
        tx.commit().await?;
        Ok(())
    }
}

async fn mutation_fence(db: &mut PgConnection, claim: &Claim) -> Result<(), DispatchError> {
    if !eligible(db, claim.operation_id).await? {
        return Err(DispatchError::Unauthorized);
    }
    crate::dispatch::fence(db, claim).await
}
