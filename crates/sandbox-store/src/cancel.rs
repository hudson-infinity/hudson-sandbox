//! Cancellation is its own operation. Only the target execution's owner may
//! interrupt the guest; cancellation claims can fence undispatched work and
//! reconcile the target's final status.
use crate::{
    Store,
    claims::Claim,
    dispatch::{self, DispatchError},
};
use sandbox_protocol::{
    DIGEST_VERSION, Id, IdempotencyKey, OperationId, ProjectId, RequestDigest, SandboxId,
    TokenKeyId,
};
use serde_json::json;
use sqlx::Row;

#[derive(Debug, Clone)]
pub struct CancelCommand {
    pub project_id: ProjectId,
    pub target: OperationId,
    pub key_id: TokenKeyId,
    pub idempotency_key: IdempotencyKey,
}
#[derive(Debug, PartialEq, Eq)]
pub enum CancelAdmission {
    Accepted {
        operation_id: OperationId,
        sandbox_id: SandboxId,
        status: String,
    },
    ResponseExpired(OperationId),
    Unauthorized,
    NotFound,
    Unsupported,
    DigestConflict,
    Busy(OperationId),
}
#[derive(Debug, PartialEq, Eq)]
pub enum CancelProgress {
    Pending,
    Completed,
}

fn result(target: OperationId, status: &str) -> serde_json::Value {
    json!({"target_operation_id":target.to_string(),"target_status":status,"cancelled":status=="cancelled"})
}
fn terminal(status: &str) -> bool {
    matches!(status, "succeeded" | "failed" | "cancelled")
}

impl Store {
    pub async fn admit_cancel(&self, r: &CancelCommand) -> Result<CancelAdmission, DispatchError> {
        match self.try_admit_cancel(r).await {
            // Create admission may race this project-wide key. The failed
            // transaction rolled back; recheck authority and the persisted key.
            Err(DispatchError::Query(error))
                if error
                    .as_database_error()
                    .and_then(sqlx::error::DatabaseError::code)
                    .is_some_and(|code| code == "23505") =>
            {
                self.try_admit_cancel(r).await
            }
            other => other,
        }
    }
    async fn try_admit_cancel(&self, r: &CancelCommand) -> Result<CancelAdmission, DispatchError> {
        let digest = RequestDigest::compute(
            "POST",
            &format!("/v1/operations/{}/cancel", r.target),
            &json!({}),
        )
        .map_err(|_| DispatchError::InvalidData)?;
        let mut tx = self.pool().begin().await?;
        // Target before project, as execution reconciliation does. Existing
        // cancel rows are read without locks; no project -> operation inversion.
        let target =
            sqlx::query("SELECT * FROM operations WHERE id=$1 AND project_id=$2 FOR UPDATE")
                .bind(r.target.uuid())
                .bind(r.project_id.uuid())
                .fetch_optional(&mut *tx)
                .await?;
        let authorized: Option<(bool,)> = sqlx::query_as(
            "SELECT status='active' AND EXISTS(
            SELECT 1 FROM jsonb_array_elements(api_tokens) t WHERE t->>'key_id'=$2
            AND (t->>'revoked_at' IS NULL OR (t->>'revoked_at')::timestamptz>clock_timestamp())
            AND (t->>'expires_at' IS NULL OR (t->>'expires_at')::timestamptz>clock_timestamp()))
            FROM projects WHERE id=$1 FOR UPDATE",
        )
        .bind(r.project_id.uuid())
        .bind(r.key_id.as_str())
        .fetch_optional(&mut *tx)
        .await?;
        if authorized != Some((true,)) {
            return Ok(CancelAdmission::Unauthorized);
        }
        let old=sqlx::query("SELECT id,sandbox_id,status,request_digest,digest_version,
            COALESCE(status IN ('succeeded','failed','cancelled') AND response_expires_at<=clock_timestamp(),false) AS expired
            FROM operations WHERE project_id=$1 AND idempotency_key=$2")
            .bind(r.project_id.uuid()).bind(r.idempotency_key.as_str()).fetch_optional(&mut *tx).await?;
        if let Some(old) = old {
            let id = OperationId::from_uuid(old.try_get("id")?);
            if old.try_get::<Vec<u8>, _>("request_digest")?.as_slice() != digest.as_bytes()
                || old.try_get::<i32, _>("digest_version")? != DIGEST_VERSION
            {
                return Ok(CancelAdmission::DigestConflict);
            }
            if old.try_get::<bool, _>("expired")? {
                return Ok(CancelAdmission::ResponseExpired(id));
            }
            return Ok(CancelAdmission::Accepted {
                operation_id: id,
                sandbox_id: SandboxId::from_uuid(old.try_get("sandbox_id")?),
                status: old.try_get("status")?,
            });
        }
        let Some(target) = target else {
            return Ok(CancelAdmission::NotFound);
        };
        if target.try_get::<String, _>("kind")? != "execute" {
            return Ok(CancelAdmission::Unsupported);
        }
        // A pre-ownership generic execute row cannot acquire invented authority.
        let target_status: String = target.try_get("status")?;
        let done = terminal(&target_status);
        if !done
            && target
                .try_get::<Option<uuid::Uuid>, _>("execution_allocation_id")?
                .is_none()
        {
            return Ok(CancelAdmission::Unsupported);
        }
        if let Some((id,))=sqlx::query_as::<_,(uuid::Uuid,)>("SELECT id FROM operations WHERE target_operation_id=$1
            AND kind='cancel' AND phase='cancel_requested' AND status IN ('queued','running','unknown')")
            .bind(r.target.uuid()).fetch_optional(&mut *tx).await? {
            return Ok(CancelAdmission::Busy(OperationId::from_uuid(id)));
        }
        let id = OperationId::generate();
        let sandbox = SandboxId::from_uuid(target.try_get("sandbox_id")?);
        let status = if done { "succeeded" } else { "queued" };
        let changed=sqlx::query("INSERT INTO operations(id,project_id,sandbox_id,kind,initiator_kind,initiator_key_id,
            idempotency_key,request_digest,digest_version,payload,target_operation_id,status,phase,result,completed_at)
            SELECT $1,$2,$3,'cancel','project',$4,$5,$6,$7,'{}',$8,$9,$10,$11,
                CASE WHEN $12 THEN clock_timestamp() ELSE NULL END
            WHERE EXISTS(SELECT 1 FROM projects p,LATERAL jsonb_array_elements(p.api_tokens) t
                WHERE p.id=$2 AND p.status='active' AND t->>'key_id'=$4
                AND (t->>'revoked_at' IS NULL OR (t->>'revoked_at')::timestamptz>clock_timestamp())
                AND (t->>'expires_at' IS NULL OR (t->>'expires_at')::timestamptz>clock_timestamp()))")
            .bind(id.uuid()).bind(r.project_id.uuid()).bind(sandbox.uuid()).bind(r.key_id.as_str())
            .bind(r.idempotency_key.as_str()).bind(digest.as_bytes().as_slice()).bind(DIGEST_VERSION)
            .bind(r.target.uuid()).bind(status).bind(if done {"cancel_complete"} else {"cancel_requested"})
            .bind(done.then(||result(r.target,&target_status))).bind(done).execute(&mut *tx).await?.rows_affected();
        if changed != 1 {
            return Err(DispatchError::Conflict);
        }
        tx.commit().await?;
        Ok(CancelAdmission::Accepted {
            operation_id: id,
            sandbox_id: sandbox,
            status: status.into(),
        })
    }

    pub async fn reconcile_cancel(&self, claim: &Claim) -> Result<CancelProgress, DispatchError> {
        let mut tx = self.pool().begin().await?;
        let row = sqlx::query(
            "SELECT * FROM operations WHERE id=$1 AND claim_revision=$2
            AND lease_expires_at>clock_timestamp() AND status IN ('running','unknown')
            AND kind='cancel' AND phase='cancel_requested' FOR UPDATE",
        )
        .bind(claim.operation_id.uuid())
        .bind(claim.revision)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(DispatchError::LostClaim)?;
        let target = OperationId::from_uuid(row.try_get("target_operation_id")?);
        let target_row=sqlx::query("SELECT status,attempt_count,attempt_receipts FROM operations WHERE id=$1 AND project_id=$2 AND sandbox_id=$3 AND kind='execute' FOR UPDATE")
            .bind(target.uuid()).bind(row.try_get::<uuid::Uuid,_>("project_id")?).bind(row.try_get::<uuid::Uuid,_>("sandbox_id")?)
            .fetch_optional(&mut *tx).await?.ok_or(DispatchError::InvalidData)?;
        let mut status: String = target_row.try_get("status")?;
        dispatch::fence(&mut tx, claim).await?;
        if matches!(status.as_str(), "queued" | "running")
            && target_row.try_get::<i32, _>("attempt_count")? == 0
            && target_row.try_get::<serde_json::Value, _>("attempt_receipts")? == json!([])
        {
            // No Dispatch action exists without its committed intent. Revoke any
            // undispatched claimant atomically, even while its host is offline.
            sqlx::query(r#"UPDATE operations SET status='cancelled',phase='cancelled_before_dispatch',
                completed_at=clock_timestamp(),lease_expires_at=NULL,next_retry_at=NULL,error=NULL,
                claim_revision=claim_revision+1,result='{"dispatch_intent_absent":true}',updated_at=clock_timestamp()
                WHERE id=$1"#).bind(target.uuid()).execute(&mut *tx).await?;
            status = "cancelled".into();
        }
        let done = terminal(&status);
        let changed=sqlx::query("UPDATE operations SET status=CASE WHEN $3 THEN 'succeeded' ELSE status END,
            phase=CASE WHEN $3 THEN 'cancel_complete' ELSE phase END,result=$4,
            completed_at=CASE WHEN $3 THEN clock_timestamp() ELSE NULL END,
            lease_expires_at=NULL,next_retry_at=CASE WHEN $3 THEN NULL ELSE clock_timestamp()+interval '1 second' END,
            updated_at=clock_timestamp() WHERE id=$1 AND claim_revision=$2 AND lease_expires_at>clock_timestamp()")
            .bind(claim.operation_id.uuid()).bind(claim.revision).bind(done).bind(done.then(||result(target,&status)))
            .execute(&mut *tx).await?.rows_affected();
        if changed != 1 {
            return Err(DispatchError::LostClaim);
        }
        tx.commit().await?;
        Ok(if done {
            CancelProgress::Completed
        } else {
            CancelProgress::Pending
        })
    }
}
