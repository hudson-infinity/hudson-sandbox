use super::*;
use crate::Store;
use sandbox_protocol::{IdempotencyKey, ProjectId, TokenHash, TokenKeyId};
use serde_json::json;
#[derive(Debug)]
pub struct UploadCommand {
    pub project_id: ProjectId,
    pub sandbox_id: SandboxId,
    pub key_id: TokenKeyId,
    pub key: IdempotencyKey,
    pub input: UploadInput,
}
#[derive(Debug)]
pub enum UploadAdmission {
    Accepted {
        plan: Box<SourcePlan>,
        status: String,
        write_source: bool,
    },
    ResponseExpired(OperationId),
    Unauthorized,
    NotFound,
    Conflict,
    NotRunning,
    Capacity,
    Invalid,
}
impl Store {
    pub async fn admit_upload(
        &self,
        r: &UploadCommand,
        hash: &TokenHash,
    ) -> Result<UploadAdmission, DispatchError> {
        if !r.input.validate() {
            return Ok(UploadAdmission::Invalid);
        }
        let digest = r.input.digest(r.sandbox_id)?;
        let mut tx = self.pool().begin().await?;
        sqlx::query("SET LOCAL statement_timeout='5s'")
            .execute(&mut *tx)
            .await?;
        sqlx::query("SELECT id FROM projects WHERE id=$1 FOR UPDATE")
            .bind(r.project_id.uuid())
            .fetch_optional(&mut *tx)
            .await?;
        let authorized:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM projects p,LATERAL jsonb_array_elements(p.api_tokens) t WHERE p.id=$1 AND p.status='active' AND t->>'key_id'=$2 AND decode(t->>'hash','hex')=$3 AND (t->>'expires_at' IS NULL OR (t->>'expires_at')::timestamptz>clock_timestamp()) AND (t->>'revoked_at' IS NULL OR (t->>'revoked_at')::timestamptz>clock_timestamp()))")
 .bind(r.project_id.uuid()).bind(r.key_id.as_str()).bind(hash.as_bytes().as_slice()).fetch_one(&mut *tx).await?;
        if !authorized {
            return Ok(UploadAdmission::Unauthorized);
        }
        if let Some(old)=sqlx::query("SELECT o.*,f.plan,f.source_frozen_at FROM operations o LEFT JOIN file_uploads f ON f.operation_id=o.id WHERE o.project_id=$1 AND o.idempotency_key=$2")
 .bind(r.project_id.uuid()).bind(r.key.as_str()).fetch_optional(&mut *tx).await? {
  if old.try_get::<Vec<u8>,_>("request_digest")?.as_slice()!=digest.as_bytes() || old.try_get::<i32,_>("digest_version")?!=sandbox_protocol::idempotency::DIGEST_VERSION{return Ok(UploadAdmission::Conflict);}
  let status:String=old.try_get("status")?;let id=OperationId::from_uuid(old.try_get("id")?);
  let now:OffsetDateTime=sqlx::query_scalar("SELECT clock_timestamp()").fetch_one(&mut *tx).await?;
  if matches!(status.as_str(),"succeeded"|"failed"|"cancelled")&&old.try_get::<Option<OffsetDateTime>,_>("response_expires_at")?.is_some_and(|t|t<=now){return Ok(UploadAdmission::ResponseExpired(id));}
  let plan:SourcePlan=serde_json::from_value(old.try_get("plan")?).map_err(|_|DispatchError::InvalidData)?;plan.validate().map_err(|_|DispatchError::InvalidData)?;
  if plan.upload.operation_id!=id || UploadInput::from_plan(&plan)!=r.input || plan.owner.scope.project_id!=r.project_id || plan.owner.scope.sandbox_id!=r.sandbox_id{return Err(DispatchError::InvalidData);}
  let write_source=old.try_get::<Option<OffsetDateTime>,_>("source_frozen_at")?.is_none()&&matches!(status.as_str(),"queued"|"running")&&old.try_get::<i32,_>("attempt_count")?==0&&ms(now)?<plan.write_expires_unix_ms;
  tx.commit().await?;return Ok(UploadAdmission::Accepted{plan:Box::new(plan),status,write_source});
 }
        let sandbox =
            sqlx::query("SELECT * FROM sandboxes WHERE id=$1 AND project_id=$2 FOR UPDATE")
                .bind(r.sandbox_id.uuid())
                .bind(r.project_id.uuid())
                .fetch_optional(&mut *tx)
                .await?;
        let Some(sandbox) = sandbox else {
            return Ok(UploadAdmission::NotFound);
        };
        if sandbox.try_get::<String, _>("desired_state")? != "running"
            || sandbox.try_get::<String, _>("observed_state")? != "running"
            || sandbox
                .try_get::<Option<uuid::Uuid>, _>("active_transition_operation_id")?
                .is_some()
        {
            return Ok(UploadAdmission::NotRunning);
        }
        let Some(allocation) = sandbox.try_get::<Option<uuid::Uuid>, _>("current_allocation_id")?
        else {
            return Ok(UploadAdmission::NotRunning);
        };
        let a=sqlx::query("SELECT a.*,h.supervisor_epoch AS current_epoch,h.status AS host_status FROM allocations a JOIN hosts h ON h.id=a.host_id WHERE a.id=$1 AND a.project_id=$2 AND a.sandbox_id=$3 FOR SHARE OF a,h")
 .bind(allocation).bind(r.project_id.uuid()).bind(r.sandbox_id.uuid()).fetch_one(&mut *tx).await?;
        if a.try_get::<i64, _>("generation")? != sandbox.try_get::<i64, _>("generation")? {
            return Ok(UploadAdmission::NotRunning);
        }
        let busy:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM operations WHERE sandbox_id=$1 AND kind='file_write' AND file_allocation_id IS NOT NULL AND status IN ('queued','running','unknown'))").bind(r.sandbox_id.uuid()).fetch_one(&mut *tx).await?;
        if busy {
            return Ok(UploadAdmission::Conflict);
        }
        // Serializes retained source reservations across projects and API replicas.
        sqlx::query("SELECT pg_advisory_xact_lock(7213091301)")
            .execute(&mut *tx)
            .await?;
        let counts=sqlx::query("SELECT count(*) FILTER(WHERE h.completed_through IS NULL OR f.operation_id>h.completed_through) AS total,COALESCE(sum(f.size) FILTER(WHERE f.source_retired_at IS NULL),0)::bigint AS bytes,count(*) FILTER(WHERE f.project_id=$1 AND (h.completed_through IS NULL OR f.operation_id>h.completed_through)) AS project_count,COALESCE(sum(f.size) FILTER(WHERE f.project_id=$1 AND f.source_retired_at IS NULL),0)::bigint AS project_bytes,count(*) FILTER(WHERE f.allocation_id=$2 AND (h.completed_through IS NULL OR f.operation_id>h.completed_through)) AS allocation_count,COALESCE(sum(f.size) FILTER(WHERE f.allocation_id=$2 AND (h.completed_through IS NULL OR f.operation_id>h.completed_through)),0)::bigint AS allocation_bytes FROM file_uploads f LEFT JOIN completed_allocation_history h ON h.allocation_id=f.allocation_id AND h.domain='files'")
 .bind(r.project_id.uuid()).bind(allocation).fetch_one(&mut *tx).await?;
        if counts.try_get::<i64, _>("total")? >= 1024
            || counts.try_get::<i64, _>("bytes")? + r.input.size as i64 > 1024 * 1024 * 1024
            || counts.try_get::<i64, _>("project_count")? >= 128
            || counts.try_get::<i64, _>("project_bytes")? + r.input.size as i64 > 256 * 1024 * 1024
            || counts.try_get::<i64, _>("allocation_count")? >= 16
            || counts.try_get::<i64, _>("allocation_bytes")? + r.input.size as i64
                > 64 * 1024 * 1024
        {
            return Ok(UploadAdmission::Capacity);
        }
        let now: OffsetDateTime = sqlx::query_scalar("SELECT clock_timestamp()")
            .fetch_one(&mut *tx)
            .await?;
        let now_ms = ms(now)?;
        let deadline = (now + time::Duration::minutes(10)).min(
            sandbox
                .try_get::<Option<OffsetDateTime>, _>("expires_at")?
                .unwrap_or(now + time::Duration::minutes(10)),
        );
        if deadline <= now {
            return Ok(UploadAdmission::NotRunning);
        }
        let id = crate::history::next_operation(&mut tx, allocation).await?;
        let plan = SourcePlan {
            version: 1,
            owner: SourceOwner {
                operation_id: id,
                scope: sandbox_protocol::file_downloads::ReadScope {
                    version: 1,
                    project_id: r.project_id,
                    sandbox_id: r.sandbox_id,
                    allocation_id: sandbox_protocol::AllocationId::from_uuid(allocation),
                    host_id: sandbox_protocol::HostId::from_uuid(a.try_get("host_id")?),
                    generation: a.try_get("generation")?,
                    host_epoch: a.try_get("supervisor_epoch")?,
                },
            },
            upload: r.input.for_operation(id),
            source_attempt: OperationId::generate(),
            created_unix_ms: now_ms,
            write_expires_unix_ms: (now_ms + 300000).min(ms(deadline)?),
            expires_unix_ms: now_ms + 3600000,
            delete_after_unix_ms: now_ms + 3600000,
        };
        plan.validate().map_err(|_| DispatchError::InvalidData)?;
        sqlx::query("INSERT INTO operations(id,project_id,sandbox_id,kind,initiator_kind,initiator_key_id,idempotency_key,request_digest,digest_version,payload,status,phase,deadline,file_allocation_id) VALUES($1,$2,$3,'file_write','project',$4,$5,$6,$7,$8,'queued','awaiting_source',$9,$10)")
 .bind(id.uuid()).bind(r.project_id.uuid()).bind(r.sandbox_id.uuid()).bind(r.key_id.as_str()).bind(r.key.as_str()).bind(digest.as_bytes().as_slice()).bind(sandbox_protocol::idempotency::DIGEST_VERSION).bind(json!(r.input)).bind(deadline).bind(allocation).execute(&mut *tx).await?;
        sqlx::query("INSERT INTO file_uploads(operation_id,project_id,sandbox_id,allocation_id,size,token_hash,plan) VALUES($1,$2,$3,$4,$5,$6,$7)").bind(id.uuid()).bind(r.project_id.uuid()).bind(r.sandbox_id.uuid()).bind(allocation).bind(r.input.size as i64).bind(hash.as_bytes().as_slice()).bind(json!(plan)).execute(&mut *tx).await?;
        if !eligible(&mut tx, id).await? {
            return Ok(UploadAdmission::NotRunning);
        }
        tx.commit().await?;
        Ok(UploadAdmission::Accepted {
            plan: Box::new(plan),
            status: "queued".into(),
            write_source: true,
        })
    }
}
