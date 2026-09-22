//! Allocation-serialized history retirement and admission accounting.
use crate::Store;
use crate::dispatch::DispatchError;
use sandbox_protocol::{
    AllocationId, HostId, Id, OperationId, ProjectId, SandboxId,
    guest_model::Context,
    history::{Barrier, Domain},
    supervisor::{
        HistoryBindingObservation, HistoryObservation, HistoryRequest, LeaseInspection,
        LeaseOwnership,
    },
};
use sqlx::postgres::PgRow;
use time::OffsetDateTime;
mod coordinator;
pub(crate) mod evidence;
mod released;
pub use coordinator::Preparation;
pub use released::{ReleasedClaim, ReleasedPreparation};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid retirement evidence")]
    Evidence,
    #[error("history claim expired or changed")]
    LostClaim,
    #[error("invalid retirement policy")]
    Policy,
    #[error("history storage failed: {0}")]
    Query(#[from] sqlx::Error),
}

#[derive(Debug, Clone)]
pub struct Claim {
    pub allocation_id: AllocationId,
    pub domain: Domain,
    pub revision: i64,
    pub expires_at: OffsetDateTime,
}
fn name(domain: Domain) -> &'static str {
    match domain {
        Domain::Commands => "commands",
        Domain::Files => "files",
    }
}
fn millis(t: OffsetDateTime) -> Result<i64, Error> {
    i64::try_from(t.unix_timestamp_nanos() / 1_000_000).map_err(|_| Error::Evidence)
}

use sqlx::{PgConnection, Row};

/// Call only after the existing project/sandbox/host admission locks. Retirement
/// locks this allocation but never waits for those outer locks or operation rows.
pub(crate) async fn next_operation(
    db: &mut PgConnection,
    allocation: uuid::Uuid,
) -> Result<OperationId, DispatchError> {
    let row =
        sqlx::query("SELECT last_admitted_operation_id FROM allocations WHERE id=$1 FOR UPDATE")
            .bind(allocation)
            .fetch_one(&mut *db)
            .await?;
    if sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(SELECT 1 FROM allocation_retirements WHERE allocation_id=$1)",
    )
    .bind(allocation)
    .fetch_one(&mut *db)
    .await?
    {
        return Err(DispatchError::Conflict);
    }
    let mut floor = row.try_get::<Option<uuid::Uuid>, _>("last_admitted_operation_id")?;
    for value in sqlx::query_scalar::<_, uuid::Uuid>(
        "SELECT reserved_through FROM allocation_history WHERE allocation_id=$1 UNION ALL SELECT reserved_through FROM released_allocation_history WHERE allocation_id=$1",
    )
    .bind(allocation)
    .fetch_all(&mut *db)
    .await?
    {
        floor = Some(floor.map_or(value, |f| f.max(value)));
    }
    let id = sandbox_protocol::history::advance_operation_id(
        OperationId::generate(),
        floor.map(OperationId::from_uuid),
    )
    .map_err(|_| DispatchError::InvalidData)?;
    sqlx::query("UPDATE allocations SET last_admitted_operation_id=$2 WHERE id=$1")
        .bind(allocation)
        .bind(id.uuid())
        .execute(db)
        .await?;
    Ok(id)
}
