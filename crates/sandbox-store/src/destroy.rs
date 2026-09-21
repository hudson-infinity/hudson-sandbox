//! Tenant-scoped destroy admission and epoch-bound stop/release evidence.
use crate::{
    Store,
    claims::Claim,
    dispatch::{self, DispatchError},
};
use sandbox_protocol::{
    Id, IdempotencyKey, OperationId, ProjectId, RequestDigest, SandboxId, TokenKeyId,
    idempotency::DIGEST_VERSION,
    supervisor::{AllocationState, Observation, Ownership},
};
use serde_json::{Value, json};
use sqlx::{PgConnection, Row};

#[derive(Debug)]
pub struct DestroySandbox {
    pub project_id: ProjectId,
    pub sandbox_id: SandboxId,
    pub key_id: TokenKeyId,
    pub idempotency_key: IdempotencyKey,
    pub request_digest: RequestDigest,
    pub correlation_id: Option<String>,
}
#[derive(Debug)]
pub enum DestroyAdmission {
    ResponseExpired(OperationId),
    Accepted {
        operation_id: OperationId,
        status: String,
    },
    NotFound,
    Unauthorized,
    DigestConflict,
    Busy(OperationId),
}
#[derive(Debug)]
pub enum DestroyAction {
    Stop(Ownership),
    Inspect(Ownership),
}

async fn existing(
    db: &mut PgConnection,
    request: &DestroySandbox,
) -> Result<Option<DestroyAdmission>, sqlx::Error> {
    let row=sqlx::query("SELECT id,request_digest,status,digest_version,
        COALESCE(status IN ('succeeded','failed','cancelled') AND response_expires_at<=clock_timestamp(),false) AS response_expired
        FROM operations WHERE project_id=$1 AND idempotency_key=$2")
        .bind(request.project_id.uuid()).bind(request.idempotency_key.as_str()).fetch_optional(db).await?;
    row.map(|row| -> Result<DestroyAdmission, sqlx::Error> {
        let digest: Vec<u8> = row.try_get("request_digest")?;
        Ok(
            if digest.as_slice() != request.request_digest.as_bytes().as_slice()
                || row.try_get::<i32, _>("digest_version")? != DIGEST_VERSION
            {
                DestroyAdmission::DigestConflict
            } else if row.try_get::<bool, _>("response_expired")? {
                DestroyAdmission::ResponseExpired(OperationId::from_uuid(row.try_get("id")?))
            } else {
                DestroyAdmission::Accepted {
                    operation_id: OperationId::from_uuid(row.try_get("id")?),
                    status: row.try_get("status")?,
                }
            },
        )
    })
    .transpose()
}

impl Store {
    pub async fn admit_destroy(
        &self,
        request: &DestroySandbox,
    ) -> Result<DestroyAdmission, DispatchError> {
        match self.try_admit_destroy(request).await {
            Err(DispatchError::Query(error))
                if error
                    .as_database_error()
                    .and_then(sqlx::error::DatabaseError::code)
                    .is_some_and(|code| code == "23505") =>
            {
                let mut connection = self.pool().acquire().await?;
                existing(&mut connection, request)
                    .await?
                    .ok_or(DispatchError::Conflict)
            }
            other => other,
        }
    }

    async fn try_admit_destroy(
        &self,
        request: &DestroySandbox,
    ) -> Result<DestroyAdmission, DispatchError> {
        let mut tx = self.pool().begin().await?;
        if let Some(result) = existing(&mut tx, request).await? {
            tx.commit().await?;
            return Ok(result);
        }
        // Discover the active operation, then lock it before project/sandbox to
        // preserve controller lock order. Recheck the pointer under the lock.
        let row = sqlx::query(
            "SELECT active_transition_operation_id FROM sandboxes WHERE id=$1 AND project_id=$2",
        )
        .bind(request.sandbox_id.uuid())
        .bind(request.project_id.uuid())
        .fetch_optional(&mut *tx)
        .await?;
        let Some(row) = row else {
            return Ok(DestroyAdmission::NotFound);
        };
        let active: Option<uuid::Uuid> = row.try_get("active_transition_operation_id")?;
        let previous = if let Some(id) = active {
            sqlx::query("SELECT * FROM operations WHERE id=$1 FOR UPDATE")
                .bind(id)
                .fetch_optional(&mut *tx)
                .await?
        } else {
            None
        };
        sqlx::query("SELECT id FROM projects WHERE id=$1 FOR UPDATE")
            .bind(request.project_id.uuid())
            .fetch_one(&mut *tx)
            .await?;
        // Serializing on the project also resolves concurrent same-key requests
        // which raced the first lookup. Create uses the unique constraint too.
        if let Some(result) = existing(&mut tx, request).await? {
            tx.commit().await?;
            return Ok(result);
        }
        let sandbox =
            sqlx::query("SELECT * FROM sandboxes WHERE id=$1 AND project_id=$2 FOR UPDATE")
                .bind(request.sandbox_id.uuid())
                .bind(request.project_id.uuid())
                .fetch_optional(&mut *tx)
                .await?
                .ok_or(DispatchError::Conflict)?;
        let current: Option<uuid::Uuid> = sandbox.try_get("active_transition_operation_id")?;
        if current != active {
            return Ok(DestroyAdmission::Busy(OperationId::from_uuid(
                current.or(active).ok_or(DispatchError::Conflict)?,
            )));
        }
        if let Some(ref previous) = previous
            && (previous.try_get::<String, _>("kind")? != "create"
                || previous.try_get::<String, _>("status")? != "unknown")
        {
            return Ok(DestroyAdmission::Busy(OperationId::from_uuid(
                active.ok_or(DispatchError::Conflict)?,
            )));
        }
        let authorized:(bool,)=sqlx::query_as("SELECT status='active' AND EXISTS(SELECT 1 FROM jsonb_array_elements(api_tokens) t
            WHERE t->>'key_id'=$2 AND (t->>'revoked_at' IS NULL OR (t->>'revoked_at')::timestamptz>clock_timestamp())
            AND (t->>'expires_at' IS NULL OR (t->>'expires_at')::timestamptz>clock_timestamp())) FROM projects WHERE id=$1")
            .bind(request.project_id.uuid()).bind(request.key_id.as_str()).fetch_one(&mut *tx).await?;
        if !authorized.0 {
            return Ok(DestroyAdmission::Unauthorized);
        }
        let allocation: Option<uuid::Uuid> = sandbox.try_get("current_allocation_id")?;
        let no_op =
            sandbox.try_get::<String, _>("observed_state")? == "destroyed" && allocation.is_none();
        let never_allocated = allocation.is_none()
            && sandbox.try_get::<i64, _>("generation")? == 0
            && previous
                .as_ref()
                .is_some_and(|op| op.try_get::<i32, _>("attempt_count").ok() == Some(0));
        if allocation.is_none() && !no_op && !never_allocated {
            return Err(DispatchError::Conflict);
        }
        let immediate = no_op || never_allocated;
        let operation = OperationId::generate();
        let payload = json!({"correlation_id":request.correlation_id,"supersedes_operation_id":active.map(|id|id.to_string())});
        sqlx::query("INSERT INTO operations(id,project_id,sandbox_id,kind,initiator_kind,initiator_key_id,idempotency_key,
            request_digest,digest_version,payload,status,phase,result,completed_at) VALUES($1,$2,$3,'destroy','project',$4,$5,$6,$7,$8,$9,$10,$11,
            CASE WHEN $12 THEN clock_timestamp() ELSE NULL END)")
            .bind(operation.uuid()).bind(request.project_id.uuid()).bind(request.sandbox_id.uuid()).bind(request.key_id.as_str())
            .bind(request.idempotency_key.as_str()).bind(request.request_digest.as_bytes().as_slice()).bind(DIGEST_VERSION).bind(payload)
            .bind(if immediate {"succeeded"} else {"queued"}).bind(if immediate {"destroyed"} else {"admitted"})
            .bind(if immediate {Some(json!({"sandbox_id":request.sandbox_id.to_string(),"noop":no_op,"never_allocated":never_allocated,
                "simulated":sandbox.try_get::<Option<bool>,_>("observation_simulated")?}))} else {None}).bind(immediate).execute(&mut *tx).await?;
        if let Some(id) = active {
            sqlx::query("UPDATE operations SET claim_revision=claim_revision+1,lease_expires_at=NULL,next_retry_at=NULL,
                phase=$2,status=$3,completed_at=CASE WHEN $4 THEN clock_timestamp() ELSE NULL END,error=$5,updated_at=clock_timestamp() WHERE id=$1")
                .bind(id).bind(if immediate {"destroyed_during_create"} else {"cleanup_owned_by_destroy"})
                .bind(if immediate {"failed"} else {"unknown"}).bind(immediate)
                .bind(json!({"code":"cleanup_owned_by_destroy","destroy_operation_id":operation.to_string(),"create_outcome_unknown":true})).execute(&mut *tx).await?;
        }
        if !no_op {
            sqlx::query("UPDATE sandboxes SET desired_state='destroyed',observed_state=$2,active_transition_operation_id=$3,
            destroyed_at=CASE WHEN $4 THEN clock_timestamp() ELSE destroyed_at END,
            observed_at=CASE WHEN $4 THEN clock_timestamp() ELSE observed_at END,state_revision=state_revision+1,updated_at=clock_timestamp() WHERE id=$1")
            .bind(request.sandbox_id.uuid()).bind(if immediate {"destroyed"} else {"destroying"})
            .bind(if immediate {None} else {Some(operation.uuid())}).bind(immediate).execute(&mut *tx).await?;
        }
        tx.commit().await?;
        Ok(DestroyAdmission::Accepted {
            operation_id: operation,
            status: if immediate { "succeeded" } else { "queued" }.into(),
        })
    }

    pub async fn prepare_destroy(
        &self,
        claim: &Claim,
        retry_stop: bool,
    ) -> Result<DestroyAction, DispatchError> {
        let mut tx = self.pool().begin().await?;
        let ctx = dispatch::context(&mut tx, claim, "destroy").await?;
        if ctx.sandbox.try_get::<String, _>("desired_state")? != "destroyed" {
            return Err(DispatchError::Conflict);
        }
        let attempts: i32 = ctx.op.try_get("attempt_count")?;
        let stop = attempts == 0 || retry_stop;
        if stop && attempts >= 100 {
            return Err(DispatchError::Conflict);
        }
        if stop {
            sqlx::query("UPDATE allocations SET status='releasing',updated_at=clock_timestamp() WHERE id=$1")
                .bind(ctx.allocation.try_get::<uuid::Uuid,_>("id")?).execute(&mut *tx).await?;
        }
        sqlx::query("UPDATE operations SET phase=$2,attempt_count=attempt_count+CASE WHEN $3 THEN 1 ELSE 0 END,
            attempt_receipts=CASE WHEN attempt_count=0 THEN attempt_receipts||jsonb_build_array($4::jsonb) ELSE attempt_receipts END,
            updated_at=clock_timestamp() WHERE id=$1")
            .bind(claim.operation_id.uuid()).bind(if stop {"stop_dispatched"} else {"reconciling"}).bind(stop)
            .bind(dispatch::evidence(&ctx.owner,None,"stop_dispatch_intent")).execute(&mut *tx).await?;
        dispatch::fence(&mut tx, claim).await?;
        tx.commit().await?;
        Ok(if stop {
            DestroyAction::Stop(ctx.owner)
        } else {
            DestroyAction::Inspect(ctx.owner)
        })
    }

    pub async fn record_destroy_unknown(&self, claim: &Claim) -> Result<(), DispatchError> {
        let mut tx = self.pool().begin().await?;
        let ctx = dispatch::context(&mut tx, claim, "destroy").await?;
        sqlx::query("UPDATE sandboxes SET observed_state='unknown',state_revision=state_revision+1,updated_at=clock_timestamp() WHERE id=$1")
            .bind(ctx.sandbox.try_get::<uuid::Uuid,_>("id")?).execute(&mut *tx).await?;
        let changed=sqlx::query("UPDATE operations SET status='unknown',phase='release_unconfirmed',lease_expires_at=NULL,
            next_retry_at=clock_timestamp()+interval '1 second',error=$3,updated_at=clock_timestamp()
            WHERE id=$1 AND claim_revision=$2 AND lease_expires_at>clock_timestamp()")
            .bind(claim.operation_id.uuid()).bind(claim.revision).bind(json!({"code":"release_unconfirmed"})).execute(&mut *tx).await?.rows_affected();
        if changed != 1 {
            return Err(DispatchError::LostClaim);
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn record_destroy_observation(
        &self,
        claim: &Claim,
        observation: &Observation,
        allow_simulated: bool,
    ) -> Result<(), DispatchError> {
        if observation.simulated && !allow_simulated {
            return Err(DispatchError::SimulationDenied);
        }
        let mut tx = self.pool().begin().await?;
        let ctx = dispatch::context(&mut tx, claim, "destroy").await?;
        if observation.ownership.as_ref() != Some(&ctx.owner)
            || ctx.op.try_get::<i32, _>("attempt_count")? < 1
            || !matches!(
                AllocationState::try_from(observation.state),
                Ok(AllocationState::Released | AllocationState::FencedAbsent)
            )
        {
            return Err(DispatchError::BadEvidence);
        }
        let mut receipt = dispatch::evidence(
            &ctx.owner,
            Some(observation.simulated),
            "allocation_released",
        );
        receipt["fenced_absent"] = json!(observation.state == AllocationState::FencedAbsent as i32);
        receipt["observed_unix_ms"] = json!(observation.observed_unix_ms);
        sqlx::query("UPDATE allocations SET status='released',released_at=clock_timestamp(),release_evidence=$2,updated_at=clock_timestamp() WHERE id=$1")
            .bind(ctx.allocation.try_get::<uuid::Uuid,_>("id")?).bind(&receipt).execute(&mut *tx).await?;
        sqlx::query("UPDATE sandboxes SET observed_state='destroyed',desired_state='destroyed',destroyed_at=clock_timestamp(),observed_at=clock_timestamp(),
            observation_simulated=$2,current_allocation_id=NULL,active_transition_operation_id=NULL,state_revision=state_revision+1,updated_at=clock_timestamp() WHERE id=$1")
            .bind(ctx.sandbox.try_get::<uuid::Uuid,_>("id")?).bind(observation.simulated).execute(&mut *tx).await?;
        let payload: Value = ctx.op.try_get("payload")?;
        if let Some(previous) = payload
            .get("supersedes_operation_id")
            .and_then(Value::as_str)
        {
            let previous: uuid::Uuid = previous.parse().map_err(|_| DispatchError::InvalidData)?;
            sqlx::query("UPDATE operations SET status='failed',phase='destroyed_during_create',completed_at=clock_timestamp(),
                error=$2,attempt_receipts=attempt_receipts||jsonb_build_array($3::jsonb),updated_at=clock_timestamp() WHERE id=$1")
                .bind(previous).bind(json!({"code":"destroyed_during_create","create_outcome_unknown":true,"destroy_operation_id":claim.operation_id.to_string()}))
                .bind(&receipt).execute(&mut *tx).await?;
        }
        let changed=sqlx::query("UPDATE operations SET status='succeeded',phase='destroyed',completed_at=clock_timestamp(),result=$3,error=NULL,
            lease_expires_at=NULL,next_retry_at=NULL,attempt_receipts=attempt_receipts||jsonb_build_array($4::jsonb),updated_at=clock_timestamp()
            WHERE id=$1 AND claim_revision=$2 AND lease_expires_at>clock_timestamp()
            AND abs(extract(epoch FROM clock_timestamp())*1000-$5::bigint)<=10000")
            .bind(claim.operation_id.uuid()).bind(claim.revision).bind(json!({"sandbox_id":ctx.owner.sandbox_id,"simulated":observation.simulated}))
            .bind(receipt).bind(observation.observed_unix_ms).execute(&mut *tx).await?.rows_affected();
        if changed != 1 {
            dispatch::fence(&mut tx, claim).await?;
            return Err(DispatchError::BadEvidence);
        }
        tx.commit().await?;
        Ok(())
    }
}
