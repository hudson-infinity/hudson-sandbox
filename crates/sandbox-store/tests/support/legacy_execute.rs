//! Seed the historical admission shape before applying newer migrations. Do not
//! run current application admission against a deliberately old database schema.
use sandbox_protocol::{AllocationId, Id, OperationId, RequestDigest};
use sandbox_store::execute::ExecuteCommand;
use sqlx::PgPool;
use time::OffsetDateTime;
pub(super) async fn seed(
    pool: &PgPool,
    r: &ExecuteCommand,
    allocation: AllocationId,
) -> OperationId {
    let id = OperationId::generate();
    let digest = RequestDigest::compute(
        "POST",
        &format!("/v1/sandboxes/{}/execute", r.sandbox_id),
        &r.command,
    )
    .unwrap();
    let deadline = OffsetDateTime::from_unix_timestamp_nanos(
        i128::from(r.command.deadline_unix_ms) * 1_000_000,
    )
    .unwrap();
    sqlx::query("INSERT INTO operations(id,project_id,sandbox_id,kind,initiator_kind,initiator_key_id,idempotency_key,request_digest,digest_version,payload,status,phase,deadline,execution_allocation_id) VALUES($1,$2,$3,'execute','project',$4,$5,$6,1,$7,'queued','admitted',$8,$9)")
        .bind(id.uuid()).bind(r.project_id.uuid()).bind(r.sandbox_id.uuid()).bind(r.key_id.as_str()).bind(r.idempotency_key.as_str()).bind(digest.as_bytes().as_slice()).bind(serde_json::json!(r.command)).bind(deadline).bind(allocation.uuid()).execute(pool).await.unwrap();
    id
}
