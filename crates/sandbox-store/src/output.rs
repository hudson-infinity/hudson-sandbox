//! Independent, fenced output publication. Never changes an execution outcome,
//! dispatch count, guest receipt, sandbox lifecycle, or resource reservation.
use crate::{Store, execute::validate_intent};
use sandbox_protocol::{
    AllocationId, HostId, Id, OperationId, ProjectId, SandboxId,
    command::CommandInput,
    guest_model::{Receipt, State},
    output::{InvalidOutput, OutputOwner, OutputPlans, OutputRefs, OutputTicket},
    supervisor::Ownership,
};
use serde_json::{Value, json};
use sqlx::{PgConnection, Row, postgres::PgRow};
use time::OffsetDateTime;

#[derive(Debug, thiserror::Error)]
pub enum OutputError {
    #[error("output claim expired or replaced")]
    LostClaim,
    #[error("invalid output retention or lease policy")]
    InvalidPolicy,
    #[error("output metadata does not match the execution receipt or saved plan")]
    BadEvidence,
    #[error("persisted output or execution evidence is invalid")]
    Corrupt,
    #[error("simulated output requires explicit development opt-in")]
    SimulationDenied,
    #[error("project deletion prevents output publication")]
    ProjectDeleting,
    #[error("output retention expired")]
    Expired,
    #[error("output storage query failed")]
    Query(#[from] sqlx::Error),
}
impl From<InvalidOutput> for OutputError {
    fn from(_: InvalidOutput) -> Self {
        Self::BadEvidence
    }
}

#[derive(Debug, Clone)]
pub struct OutputClaim {
    pub operation_id: OperationId,
    pub revision: i64,
    pub lease_expires_at: OffsetDateTime,
}

#[derive(Clone)]
pub struct OutputWork {
    pub ticket: OutputTicket,
    pub plans: Option<OutputPlans>,
    pub receipt: Receipt,
    pub simulated: bool,
}
impl std::fmt::Debug for OutputWork {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OutputWork")
            .field("ticket", &self.ticket)
            .field("simulated", &self.simulated)
            .finish_non_exhaustive()
    }
}

/// Internal lookup result. API responses must not expose private references.
#[derive(Debug, Clone)]
pub struct OutputView {
    pub simulated: Option<bool>,
    pub status: String,
    pub owner: Option<OutputOwner>,
    pub references: Option<OutputRefs>,
    pub expires_at: Option<OffsetDateTime>,
}

fn ms(time: OffsetDateTime) -> Result<i64, OutputError> {
    i64::try_from(time.unix_timestamp_nanos() / 1_000_000).map_err(|_| OutputError::Corrupt)
}
fn lease(seconds: u32) -> Result<i32, OutputError> {
    if !(1..=300).contains(&seconds) {
        return Err(OutputError::InvalidPolicy);
    }
    Ok(seconds as i32)
}
async fn claimed(db: &mut PgConnection, claim: &OutputClaim) -> Result<PgRow, OutputError> {
    let row = sqlx::query("SELECT * FROM operations WHERE id=$1 AND output_claim_revision=$2
        AND output_lease_expires_at>clock_timestamp() AND output_status IN ('pending','uploading') FOR UPDATE")
        .bind(claim.operation_id.uuid()).bind(claim.revision).fetch_optional(&mut *db).await?
        .ok_or(OutputError::LostClaim)?;
    // Same lock order as operation reconciliation. No allocation/host locks
    // are needed: the historical pinned tuple must survive destroy and reboot.
    let (status,): (String,) = sqlx::query_as("SELECT status FROM projects WHERE id=$1 FOR UPDATE")
        .bind(row.try_get::<uuid::Uuid, _>("project_id")?)
        .fetch_one(&mut *db)
        .await?;
    if status == "deleting" {
        return Err(OutputError::ProjectDeleting);
    }
    Ok(row)
}
async fn fence(db: &mut PgConnection, claim: &OutputClaim) -> Result<(), OutputError> {
    let (valid,): (bool,) = sqlx::query_as(
        "SELECT output_claim_revision=$2 AND output_lease_expires_at>clock_timestamp()
        AND output_status IN ('pending','uploading') FROM operations WHERE id=$1",
    )
    .bind(claim.operation_id.uuid())
    .bind(claim.revision)
    .fetch_one(db)
    .await?;
    if !valid {
        return Err(OutputError::LostClaim);
    }
    Ok(())
}

pub(crate) struct Evidence {
    pub(crate) owner: OutputOwner,
    pub(crate) receipt: Receipt,
    pub(crate) simulated: bool,
}
pub(crate) async fn execution_evidence(
    db: &mut PgConnection,
    row: &PgRow,
) -> Result<Evidence, OutputError> {
    let operation_id = OperationId::from_uuid(row.try_get("id")?);
    let project_id = ProjectId::from_uuid(row.try_get("project_id")?);
    let sandbox_id = SandboxId::from_uuid(row.try_get("sandbox_id")?);
    let allocation: uuid::Uuid = row
        .try_get::<Option<uuid::Uuid>, _>("execution_allocation_id")?
        .ok_or(OutputError::Corrupt)?;
    let pinned = sqlx::query("SELECT host_id,generation,supervisor_epoch FROM allocations WHERE id=$1 AND project_id=$2 AND sandbox_id=$3")
        .bind(allocation).bind(project_id.uuid()).bind(sandbox_id.uuid()).fetch_optional(&mut *db).await?.ok_or(OutputError::Corrupt)?;
    let command: CommandInput =
        serde_json::from_value(row.try_get("payload")?).map_err(|_| OutputError::Corrupt)?;
    command.validate().map_err(|_| OutputError::Corrupt)?;
    let command = command.for_operation(operation_id);
    let digest = command.digest().map_err(|_| OutputError::Corrupt)?;
    let owner = Ownership {
        project_id: project_id.to_string(),
        sandbox_id: sandbox_id.to_string(),
        operation_id: operation_id.to_string(),
        allocation_id: AllocationId::from_uuid(allocation).to_string(),
        host_id: HostId::from_uuid(pinned.try_get("host_id")?).to_string(),
        generation: pinned.try_get("generation")?,
        supervisor_epoch: pinned.try_get("supervisor_epoch")?,
        claim_revision: 0,
        claim_expires_unix_ms: 0,
    };
    validate_intent(row, &owner, &digest).map_err(|_| OutputError::Corrupt)?;
    let history: Value = row.try_get("attempt_receipts")?;
    if history.as_array().is_none_or(|v| v.len() != 2) {
        return Err(OutputError::Corrupt);
    }
    let observed = &history[1];
    if observed["phase"] != "command_observed"
        || [
            "project_id",
            "sandbox_id",
            "operation_id",
            "allocation_id",
            "host_id",
            "generation",
            "supervisor_epoch",
        ]
        .iter()
        .any(|key| observed.get(*key) != history[0].get(*key))
    {
        return Err(OutputError::Corrupt);
    }
    let simulated = observed["simulated"]
        .as_bool()
        .ok_or(OutputError::Corrupt)?;
    let receipt: Receipt = serde_json::from_value(observed["guest_receipt"].clone())
        .map_err(|_| OutputError::Corrupt)?;
    receipt.validate().map_err(|_| OutputError::Corrupt)?;
    if row.try_get::<String, _>("kind")? != "execute"
        || receipt.operation_id != operation_id
        || receipt.context.allocation_id.uuid() != allocation
        || receipt.context.generation != owner.generation
        || receipt.digest != digest
        || receipt.output_limit != command.output_limit
        || receipt.deadline_unix_ms != command.deadline_unix_ms
    {
        return Err(OutputError::Corrupt);
    }
    let output_owner = OutputOwner {
        project_id,
        sandbox_id,
        operation_id,
        allocation_id: receipt.context.allocation_id,
        generation: owner.generation,
        host_id: HostId::from_uuid(pinned.try_get("host_id")?),
        host_epoch: owner.supervisor_epoch,
        boot_id: receipt.context.boot_id.clone(),
    };
    Ok(Evidence {
        owner: output_owner,
        receipt,
        simulated,
    })
}

pub(crate) async fn evidence(db: &mut PgConnection, row: &PgRow) -> Result<Evidence, OutputError> {
    let evidence = execution_evidence(db, row).await?;
    let receipt = &evidence.receipt;
    let (status, phase) = match receipt.state {
        State::Exited if receipt.exit_code == Some(0) => ("succeeded", "exited"),
        State::Exited => ("failed", "exited"),
        State::TimedOut => ("failed", "timed_out"),
        State::Cancelled => ("cancelled", "cancelled"),
        _ => return Err(OutputError::Corrupt),
    };
    if row.try_get::<String, _>("status")? != status
        || row.try_get::<Option<String>, _>("phase")?.as_deref() != Some(phase)
        || row
            .try_get::<Option<OffsetDateTime>, _>("completed_at")?
            .is_none()
    {
        return Err(OutputError::Corrupt);
    }
    Ok(evidence)
}

fn validate_plans(
    ticket: &OutputTicket,
    plans: &OutputPlans,
    e: &Evidence,
) -> Result<(), OutputError> {
    ticket.validate_plans(plans)?;
    for (plan, stats) in [
        (&plans.stdout, &e.receipt.stdout),
        (&plans.stderr, &e.receipt.stderr),
    ] {
        if plan.size != stats.stored || plan.seen != stats.seen || plan.truncated != stats.truncated
        {
            return Err(OutputError::BadEvidence);
        }
    }
    Ok(())
}
pub(crate) fn saved(row: &PgRow, e: &Evidence) -> Result<Option<OutputWork>, OutputError> {
    let Some(value) = row.try_get::<Option<Value>, _>("output_ticket")? else {
        return Ok(None);
    };
    let ticket: OutputTicket = serde_json::from_value(value).map_err(|_| OutputError::Corrupt)?;
    ticket.validate().map_err(|_| OutputError::Corrupt)?;
    if ticket.owner != e.owner
        || ticket.output_limit != e.receipt.output_limit
        || Some(ticket.expires_unix_ms)
            != row
                .try_get::<Option<OffsetDateTime>, _>("output_expires_at")?
                .map(ms)
                .transpose()?
    {
        return Err(OutputError::Corrupt);
    }
    let plans = row
        .try_get::<Option<Value>, _>("output_plan")?
        .map(serde_json::from_value::<OutputPlans>)
        .transpose()
        .map_err(|_| OutputError::Corrupt)?;
    if let Some(plans) = &plans {
        validate_plans(&ticket, plans, e).map_err(|_| OutputError::Corrupt)?;
    }
    Ok(Some(OutputWork {
        ticket,
        plans,
        receipt: e.receipt.clone(),
        simulated: e.simulated,
    }))
}

impl Store {
    pub async fn claim_output(&self, seconds: u32) -> Result<Option<OutputClaim>, OutputError> {
        let seconds = lease(seconds)?;
        let row = sqlx::query("WITH candidate AS (SELECT id FROM operations
            WHERE output_status IN ('pending','uploading')
            AND (output_lease_expires_at IS NULL OR output_lease_expires_at<=clock_timestamp())
            AND (output_next_retry_at IS NULL OR output_next_retry_at<=clock_timestamp())
            ORDER BY completed_at,id FOR UPDATE SKIP LOCKED LIMIT 1)
            UPDATE operations o SET output_claim_revision=o.output_claim_revision+1,
            output_lease_expires_at=clock_timestamp()+make_interval(secs=>$1),output_next_retry_at=NULL
            FROM candidate c WHERE o.id=c.id RETURNING o.id,o.output_claim_revision,o.output_lease_expires_at")
            .bind(seconds).fetch_optional(self.pool()).await?;
        row.map(|r| {
            Ok(OutputClaim {
                operation_id: OperationId::from_uuid(r.try_get("id")?),
                revision: r.try_get("output_claim_revision")?,
                lease_expires_at: r.try_get("output_lease_expires_at")?,
            })
        })
        .transpose()
    }

    /// Reconstruct historical command ownership. Current allocation pointers,
    /// host epochs, token revocation and sandbox destruction never redirect it.
    /// Retention starts at command completion, not at a later retry.
    pub async fn prepare_output(
        &self,
        claim: &OutputClaim,
        retention_seconds: u32,
        cleanup_grace_seconds: u32,
        allow_simulated: bool,
    ) -> Result<OutputWork, OutputError> {
        if !(1..=30 * 24 * 3600).contains(&retention_seconds)
            || cleanup_grace_seconds > 7 * 24 * 3600
        {
            return Err(OutputError::InvalidPolicy);
        }
        let mut tx = self.pool().begin().await?;
        let row = claimed(&mut tx, claim).await?;
        let e = evidence(&mut tx, &row).await?;
        if e.simulated && !allow_simulated {
            return Err(OutputError::SimulationDenied);
        }
        let work = if let Some(work) = saved(&row, &e)? {
            work
        } else {
            let created = ms(row.try_get("completed_at")?)?;
            let mut expires = created
                .checked_add(i64::from(retention_seconds) * 1000)
                .ok_or(OutputError::Corrupt)?;
            if let Some(response) =
                row.try_get::<Option<OffsetDateTime>, _>("response_expires_at")?
            {
                expires = expires.min(ms(response)?);
            }
            // A response already expired before execution finished cannot grant
            // a new retention window. Keep a valid, immediately expired ticket.
            expires = expires.max(created + 1);
            OutputWork {
                ticket: OutputTicket {
                    version: 1,
                    owner: e.owner.clone(),
                    upload_attempt: OperationId::generate(),
                    output_limit: e.receipt.output_limit,
                    created_unix_ms: created,
                    expires_unix_ms: expires,
                    delete_after_unix_ms: expires
                        .checked_add(i64::from(cleanup_grace_seconds) * 1000)
                        .ok_or(OutputError::Corrupt)?,
                },
                plans: None,
                receipt: e.receipt.clone(),
                simulated: e.simulated,
            }
        };
        work.ticket.validate()?;
        fence(&mut tx, claim).await?;
        let expiry = OffsetDateTime::from_unix_timestamp_nanos(
            i128::from(work.ticket.expires_unix_ms) * 1_000_000,
        )
        .map_err(|_| OutputError::Corrupt)?;
        let (status,): (String,) = sqlx::query_as("UPDATE operations SET output_ticket=$3,
            output_expires_at=$4,
            output_status=CASE WHEN (clock_timestamp()>=$4 OR response_expires_at<=clock_timestamp()) THEN 'expired' ELSE 'uploading' END,
            output_lease_expires_at=CASE WHEN (clock_timestamp()>=$4 OR response_expires_at<=clock_timestamp()) THEN NULL ELSE output_lease_expires_at END
            WHERE id=$1 AND output_claim_revision=$2 AND output_lease_expires_at>clock_timestamp()
            RETURNING output_status")
            .bind(claim.operation_id.uuid()).bind(claim.revision).bind(json!(work.ticket)).bind(expiry)
            .fetch_optional(&mut *tx).await?.ok_or(OutputError::LostClaim)?;
        tx.commit().await?;
        if status == "expired" {
            return Err(OutputError::Expired);
        }
        Ok(work)
    }

    pub async fn save_output_plans(
        &self,
        claim: &OutputClaim,
        plans: &OutputPlans,
        allow_simulated: bool,
    ) -> Result<(), OutputError> {
        let mut tx = self.pool().begin().await?;
        let row = claimed(&mut tx, claim).await?;
        let e = evidence(&mut tx, &row).await?;
        if e.simulated && !allow_simulated {
            return Err(OutputError::SimulationDenied);
        }
        let work = saved(&row, &e)?.ok_or(OutputError::BadEvidence)?;
        validate_plans(&work.ticket, plans, &e)?;
        if work.plans.as_ref().is_some_and(|old| old != plans) {
            return Err(OutputError::BadEvidence);
        }
        fence(&mut tx, claim).await?;
        let changed = sqlx::query(
            "UPDATE operations SET output_plan=$3 WHERE id=$1 AND output_claim_revision=$2
            AND output_lease_expires_at>clock_timestamp() AND output_expires_at>clock_timestamp() AND (response_expires_at IS NULL OR response_expires_at>clock_timestamp())",
        )
        .bind(claim.operation_id.uuid())
        .bind(claim.revision)
        .bind(json!(plans))
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if changed != 1 {
            fence(&mut tx, claim).await?;
            return Err(OutputError::Expired);
        }
        tx.commit().await?;
        Ok(())
    }

    /// Metadata only, supplied by an authenticated archiver after verified
    /// uploads. Object existence/integrity must still be checked on byte reads.
    pub async fn publish_output(
        &self,
        claim: &OutputClaim,
        refs: &OutputRefs,
        allow_simulated: bool,
    ) -> Result<(), OutputError> {
        let mut tx = self.pool().begin().await?;
        let row = claimed(&mut tx, claim).await?;
        let e = evidence(&mut tx, &row).await?;
        if e.simulated && !allow_simulated {
            return Err(OutputError::SimulationDenied);
        }
        let work = saved(&row, &e)?.ok_or(OutputError::BadEvidence)?;
        refs.validate(&e.owner, e.receipt.output_limit)?;
        validate_plans(&work.ticket, &refs.plans(), &e)?;
        if work.plans.as_ref() != Some(&refs.plans()) {
            return Err(OutputError::BadEvidence);
        }
        fence(&mut tx, claim).await?;
        let changed = sqlx::query(
            "UPDATE operations SET output_refs=$3,output_status='published',
            output_lease_expires_at=NULL,output_next_retry_at=NULL
            WHERE id=$1 AND output_claim_revision=$2 AND output_lease_expires_at>clock_timestamp()
            AND output_expires_at>clock_timestamp() AND (response_expires_at IS NULL OR response_expires_at>clock_timestamp())",
        )
        .bind(claim.operation_id.uuid())
        .bind(claim.revision)
        .bind(json!([refs.stdout, refs.stderr]))
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if changed != 1 {
            fence(&mut tx, claim).await?;
            return Err(OutputError::Expired);
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn renew_output_claim(
        &self,
        claim: &OutputClaim,
        seconds: u32,
    ) -> Result<OffsetDateTime, OutputError> {
        let seconds = lease(seconds)?;
        let (until,) = sqlx::query_as("UPDATE operations SET output_lease_expires_at=clock_timestamp()+make_interval(secs=>$3)
            WHERE id=$1 AND output_claim_revision=$2 AND output_lease_expires_at>clock_timestamp()
            AND output_status IN ('pending','uploading') RETURNING output_lease_expires_at")
            .bind(claim.operation_id.uuid()).bind(claim.revision).bind(seconds)
            .fetch_optional(self.pool()).await?.ok_or(OutputError::LostClaim)?;
        Ok(until)
    }

    pub async fn defer_output(&self, claim: &OutputClaim, seconds: u32) -> Result<(), OutputError> {
        if !(1..=3600).contains(&seconds) {
            return Err(OutputError::InvalidPolicy);
        }
        let n = sqlx::query(
            "UPDATE operations SET output_lease_expires_at=NULL,
            output_next_retry_at=clock_timestamp()+make_interval(secs=>$3)
            WHERE id=$1 AND output_claim_revision=$2 AND output_lease_expires_at>clock_timestamp()
            AND output_status IN ('pending','uploading')",
        )
        .bind(claim.operation_id.uuid())
        .bind(claim.revision)
        .bind(seconds as i32)
        .execute(self.pool())
        .await?
        .rows_affected();
        if n != 1 {
            return Err(OutputError::LostClaim);
        }
        Ok(())
    }

    /// Tenant-scoped internal references. Caller authenticates the token before
    /// this query and again before delivering bytes after a slow storage read.
    pub async fn output_for_project(
        &self,
        project: ProjectId,
        operation: OperationId,
    ) -> Result<Option<OutputView>, OutputError> {
        let mut tx = self.pool().begin().await?;
        let row = sqlx::query("SELECT o.*,clock_timestamp() AS read_now FROM operations o JOIN projects p ON p.id=o.project_id
            WHERE o.id=$1 AND o.project_id=$2 AND p.status='active'")
            .bind(operation.uuid()).bind(project.uuid()).fetch_optional(&mut *tx).await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let mut status: String = row.try_get("output_status")?;
        let output_expires: Option<OffsetDateTime> = row.try_get("output_expires_at")?;
        let response_expires: Option<OffsetDateTime> = row.try_get("response_expires_at")?;
        let expires_at = match (output_expires, response_expires) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        let now: OffsetDateTime = row.try_get("read_now")?;
        if status == "none" {
            if expires_at.is_some_and(|at| at <= now) {
                status = "expired".into();
            }
            return Ok(Some(OutputView {
                simulated: None,
                status,
                owner: None,
                references: None,
                expires_at,
            }));
        }
        let e = evidence(&mut tx, &row).await?;
        let work = saved(&row, &e)?;
        let references = if status == "published" {
            let work = work.ok_or(OutputError::Corrupt)?;
            let raw: Value = row.try_get("output_refs")?;
            let items = raw
                .as_array()
                .filter(|a| a.len() == 2)
                .ok_or(OutputError::Corrupt)?;
            let refs = OutputRefs {
                stdout: serde_json::from_value(items[0].clone())
                    .map_err(|_| OutputError::Corrupt)?,
                stderr: serde_json::from_value(items[1].clone())
                    .map_err(|_| OutputError::Corrupt)?,
            };
            refs.validate(&e.owner, e.receipt.output_limit)
                .map_err(|_| OutputError::Corrupt)?;
            if work.plans.as_ref() != Some(&refs.plans()) {
                return Err(OutputError::Corrupt);
            }
            Some(refs)
        } else {
            None
        };
        if expires_at.is_some_and(|at| at <= now) {
            status = "expired".into();
        }
        Ok(Some(OutputView {
            simulated: Some(e.simulated),
            references: if status == "published" {
                references
            } else {
                None
            },
            status,
            owner: Some(e.owner),
            expires_at,
        }))
    }
}
