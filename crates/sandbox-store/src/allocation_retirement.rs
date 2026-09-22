//! Database freeze and renewable claims only. No host deletion is authorized by
//! this module; the host must independently fence, verify and acknowledge cleanup.
use crate::Store;
use sandbox_protocol::{
    AllocationId, HostId, Id, OperationId, ProjectId, SandboxId,
    allocation_authority::Permit,
    allocation_retirement::{DomainClosure, Intent, Request},
    history::Domain,
};
use serde_json::Value;
use sha2::{Digest, Sha256};
use sqlx::{PgConnection, Row, postgres::PgRow};
use time::OffsetDateTime;
mod evidence;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("allocation retirement is not eligible")]
    Ineligible,
    #[error("invalid allocation retirement evidence")]
    Evidence,
    #[error("invalid allocation retirement policy")]
    Policy,
    #[error("allocation retirement storage failed: {0}")]
    Query(#[from] sqlx::Error),
}
impl From<crate::history::Error> for Error {
    fn from(e: crate::history::Error) -> Self {
        match e {
            crate::history::Error::Query(e) => Self::Query(e),
            _ => Self::Evidence,
        }
    }
}
impl Store {
    /// Freeze the complete scope under the allocation admission lock. A held
    /// claim returns None. Retries retain the original intent and identity.
    /// Callers must retain this row through the later acknowledgement protocol.
    pub async fn prepare_allocation_retirement(
        &self,
        allocation: AllocationId,
        host: HostId,
        epoch: i64,
        seconds: u32,
        allow_simulated: bool,
    ) -> Result<Option<Request>, Error> {
        if epoch <= 0 || !(1..=300).contains(&seconds) {
            return Err(Error::Policy);
        }
        let mut tx = self.pool().begin().await?;
        sqlx::query("SET LOCAL statement_timeout='5s'")
            .execute(&mut *tx)
            .await?;
        // Do not lock project/sandbox/host/operation rows after this lock:
        // admission and reconciliation acquire those locks in the other order.
        let a = sqlx::query("SELECT * FROM allocations WHERE id=$1 FOR UPDATE")
            .bind(allocation.uuid())
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(Error::Ineligible)?;
        let p = sqlx::query("SELECT p.* FROM allocation_permits p JOIN hosts h ON h.id=p.host_id WHERE p.allocation_id=$1 AND p.host_id=$2 AND h.supervisor_epoch=$3 AND h.launch_authority_required AND h.registered_allocation_serial>=p.serial")
            .bind(allocation.uuid()).bind(host.uuid()).bind(epoch).fetch_optional(&mut *tx).await?.ok_or(Error::Ineligible)?;
        let permit = Permit {
            host,
            allocation,
            project: ProjectId::from_uuid(p.try_get("project_id")?),
            sandbox: SandboxId::from_uuid(p.try_get("sandbox_id")?),
            create_operation: OperationId::from_uuid(p.try_get("create_operation_id")?),
            generation: p.try_get("generation")?,
            original_epoch: p.try_get("original_epoch")?,
            serial: u64::try_from(p.try_get::<i64, _>("serial")?).map_err(|_| Error::Evidence)?,
        };
        if a.try_get::<uuid::Uuid, _>("host_id")? != host.uuid()
            || a.try_get::<uuid::Uuid, _>("project_id")? != permit.project.uuid()
            || a.try_get::<uuid::Uuid, _>("sandbox_id")? != permit.sandbox.uuid()
            || a.try_get::<i64, _>("generation")? != permit.generation
            || a.try_get::<i64, _>("supervisor_epoch")? != permit.original_epoch
            || permit.original_epoch > epoch
        {
            return Err(Error::Evidence);
        }
        let old = sqlx::query("SELECT *,lease_expires_at>clock_timestamp() AS busy FROM allocation_retirements WHERE allocation_id=$1")
            .bind(allocation.uuid()).fetch_optional(&mut *tx).await?;
        if old
            .as_ref()
            .is_some_and(|r| r.get::<Option<bool>, _>("busy") == Some(true))
        {
            tx.rollback().await?;
            return Ok(None);
        }
        let retirement = old
            .as_ref()
            .map(|r| {
                r.try_get::<uuid::Uuid, _>("retirement_id")
                    .map(OperationId::from_uuid)
            })
            .transpose()?
            .unwrap_or_else(OperationId::generate);
        let (release, simulated) =
            evidence::release(&mut tx, &a, &permit, allow_simulated, epoch).await?;
        evidence::consumers(&mut tx, &permit).await?;
        let (commands, commands_simulated) =
            evidence::domain(&mut tx, &a, Domain::Commands, allow_simulated).await?;
        let (files, files_simulated) =
            evidence::domain(&mut tx, &a, Domain::Files, allow_simulated).await?;
        let intent = Intent {
            version: 1,
            retirement,
            permit,
            commands,
            files,
            simulated: allow_simulated || simulated || commands_simulated || files_simulated,
            release_evidence_sha256: hex::encode(Sha256::digest(
                serde_json::to_vec(&release).map_err(|_| Error::Evidence)?,
            )),
        };
        intent.validate().map_err(|_| Error::Evidence)?;
        if let Some(old) = &old {
            let retained: Intent =
                serde_json::from_value(old.try_get("intent")?).map_err(|_| Error::Evidence)?;
            if retained != intent {
                return Err(Error::Evidence);
            }
        } else {
            sqlx::query("INSERT INTO allocation_retirements(allocation_id,retirement_id,intent,reporting_epoch) VALUES($1,$2,$3,$4)")
                .bind(allocation.uuid()).bind(retirement.uuid()).bind(serde_json::to_value(&intent).map_err(|_|Error::Evidence)?).bind(epoch).execute(&mut *tx).await?;
        }
        let row=sqlx::query("UPDATE allocation_retirements r SET claim_revision=claim_revision+1,reporting_epoch=$2,lease_expires_at=clock_timestamp()+make_interval(secs=>$3) WHERE allocation_id=$1 AND EXISTS(SELECT 1 FROM hosts h WHERE h.id=$4 AND h.supervisor_epoch=$2 AND h.launch_authority_required AND h.registered_allocation_serial>=$5) RETURNING claim_revision,lease_expires_at")
            .bind(allocation.uuid()).bind(epoch).bind(f64::from(seconds)).bind(host.uuid()).bind(intent.permit.serial as i64).fetch_optional(&mut *tx).await?.ok_or(Error::Ineligible)?;
        let expires: OffsetDateTime = row.try_get("lease_expires_at")?;
        let request = Request {
            intent,
            reporting_epoch: epoch,
            revision: row.try_get("claim_revision")?,
            expires_unix_ms: i64::try_from(expires.unix_timestamp_nanos() / 1_000_000)
                .map_err(|_| Error::Evidence)?,
        };
        tx.commit().await?;
        Ok(Some(request))
    }
}
