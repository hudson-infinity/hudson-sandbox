//! Synthetic release observations exercise database admission, not VM cleanup.
use super::*;
use sandbox_protocol::{allocation_retirement::DomainClosure, history::Domain};
use sandbox_store::allocation_retirement::Error;

async fn released(f: &Fixture) {
    let id = OperationId::generate();
    let receipt = json!({"phase":"allocation_released","operation_id":id.to_string(),"host_id":f.host.to_string(),"project_id":f.request.project_id.to_string(),"sandbox_id":f.request.sandbox_id.to_string(),"allocation_id":f.allocation.to_string(),"generation":1,"supervisor_epoch":1,"claim_revision":1,"simulated":false,"observed_unix_ms":now_ms(),"start_count":0});
    sqlx::query("INSERT INTO operations(id,project_id,sandbox_id,kind,initiator_kind,idempotency_key,request_digest,digest_version,payload,status,phase,completed_at,claim_revision,attempt_count,attempt_receipts) VALUES($1,$2,$3,'create','service',$4,$5,1,'{}','failed','allocation_released',clock_timestamp(),1,1,jsonb_build_array($6::jsonb))")
        .bind(id.uuid()).bind(f.request.project_id.uuid()).bind(f.request.sandbox_id.uuid()).bind(id.to_string()).bind(vec![0u8;32]).bind(&receipt).execute(f.store.pool()).await.unwrap();
    sqlx::query("UPDATE hosts SET last_allocation_serial=1,registered_allocation_serial=1,launch_authority_required=true WHERE id=$1").bind(f.host.uuid()).execute(f.store.pool()).await.unwrap();
    sqlx::query("INSERT INTO allocation_permits(allocation_id,host_id,serial,project_id,sandbox_id,create_operation_id,generation,original_epoch) VALUES($1,$2,1,$3,$4,$5,1,1)")
        .bind(f.allocation.uuid()).bind(f.host.uuid()).bind(f.request.project_id.uuid()).bind(f.request.sandbox_id.uuid()).bind(id.uuid()).execute(f.store.pool()).await.unwrap();
    sqlx::query("UPDATE allocations SET status='released',released_at=clock_timestamp(),release_evidence=$2 WHERE id=$1").bind(f.allocation.uuid()).bind(receipt).execute(f.store.pool()).await.unwrap();
    sqlx::query("UPDATE sandboxes SET desired_state='destroyed',observed_state='destroyed',current_allocation_id=NULL WHERE id=$1").bind(f.request.sandbox_id.uuid()).execute(f.store.pool()).await.unwrap();
}
async fn prepare(
    f: &Fixture,
) -> Result<Option<sandbox_protocol::allocation_retirement::Request>, Error> {
    f.store
        .prepare_allocation_retirement(f.allocation, f.host, 1, 30, false)
        .await
}
async fn expire(f: &Fixture) {
    sqlx::query("UPDATE allocation_retirements SET lease_expires_at=clock_timestamp()-interval '1 second' WHERE allocation_id=$1").bind(f.allocation.uuid()).execute(f.store.pool()).await.unwrap();
}
async fn history_done(f: &Fixture) {
    let p = f
        .store
        .claim_released_history(f.host, 1, Domain::Commands, 30, false)
        .await
        .unwrap()
        .unwrap();
    let observed = sandbox_protocol::supervisor::ReleasedHistoryObservation {
        completed_through: p.request.through.clone(),
        request: Some(p.request),
        release_state: sandbox_protocol::supervisor::AllocationState::Released as i32,
        simulated: false,
        observed_unix_ms: now_ms(),
    };
    f.store
        .complete_released_history(&p.claim, &observed, false)
        .await
        .unwrap();
}
#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn empty_scope_is_explicit_and_retry_keeps_identity(pool: PgPool) {
    let f = Fixture::new(&pool).await;
    released(&f).await;
    let before: serde_json::Value =
        sqlx::query_scalar("SELECT jsonb_agg(to_jsonb(o) ORDER BY id) FROM operations o")
            .fetch_one(&pool)
            .await
            .unwrap();
    let first = prepare(&f).await.unwrap().unwrap();
    assert_eq!(first.intent.commands, DomainClosure::Empty {});
    assert_eq!(first.intent.files, DomainClosure::Empty {});
    assert!(prepare(&f).await.unwrap().is_none());
    expire(&f).await;
    sqlx::query("UPDATE hosts SET supervisor_epoch=2 WHERE id=$1")
        .bind(f.host.uuid())
        .execute(&pool)
        .await
        .unwrap();
    assert!(matches!(prepare(&f).await, Err(Error::Ineligible)));
    let next = f
        .store
        .prepare_allocation_retirement(f.allocation, f.host, 2, 30, false)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(next.intent, first.intent);
    assert_eq!(next.revision, first.revision + 1);
    assert_eq!(next.reporting_epoch, 2);
    let after: serde_json::Value =
        sqlx::query_scalar("SELECT jsonb_agg(to_jsonb(o) ORDER BY id) FROM operations o")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(before, after);
}
#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn unknown_outcomes_and_unacknowledged_domains_prevent_freeze(pool: PgPool) {
    let f = Fixture::new(&pool).await;
    let command = f.admit().await;
    released(&f).await;
    assert!(matches!(prepare(&f).await, Err(Error::Ineligible)));
    super::history::finish(&f, command).await;
    assert!(matches!(prepare(&f).await, Err(Error::Ineligible)));
    history_done(&f).await;
    let p = prepare(&f).await.unwrap().unwrap();
    assert_eq!(
        p.intent.commands,
        DomainClosure::Retired { through: command }
    );
    assert!(
        f.store
            .claim_released_history(f.host, 1, Domain::Commands, 30, false)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        f.store.admit_execute(&f.request).await.unwrap(),
        ExecuteAdmission::Accepted {
            operation_id: command,
            status: "failed".into()
        }
    );
}
#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn changed_frozen_evidence_does_not_replace_intent(pool: PgPool) {
    let f = Fixture::new(&pool).await;
    released(&f).await;
    let first = prepare(&f).await.unwrap().unwrap();
    expire(&f).await;
    sqlx::query("UPDATE allocations SET release_evidence=jsonb_set(release_evidence,'{observed_unix_ms}',to_jsonb(123::bigint)) WHERE id=$1").bind(f.allocation.uuid()).execute(&pool).await.unwrap();
    assert!(matches!(prepare(&f).await, Err(Error::Evidence)));
    let retained: serde_json::Value =
        sqlx::query_scalar("SELECT intent FROM allocation_retirements WHERE allocation_id=$1")
            .bind(f.allocation.uuid())
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(retained, serde_json::to_value(first.intent).unwrap());
}
#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn ownership_registration_and_claims_are_required(pool: PgPool) {
    let f = Fixture::new(&pool).await;
    assert!(matches!(prepare(&f).await, Err(Error::Ineligible)));
    released(&f).await;
    assert!(matches!(
        f.store
            .prepare_allocation_retirement(f.allocation, HostId::generate(), 1, 30, false)
            .await,
        Err(Error::Ineligible)
    ));
    sqlx::query("UPDATE hosts SET registered_allocation_serial=0 WHERE id=$1")
        .bind(f.host.uuid())
        .execute(&pool)
        .await
        .unwrap();
    assert!(matches!(prepare(&f).await, Err(Error::Ineligible)));
    sqlx::query("UPDATE hosts SET registered_allocation_serial=1 WHERE id=$1")
        .bind(f.host.uuid())
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE allocations SET maintenance_lease_until=clock_timestamp()+interval '30 seconds',maintenance_revision=1 WHERE id=$1").bind(f.allocation.uuid()).execute(&pool).await.unwrap();
    assert!(matches!(prepare(&f).await, Err(Error::Ineligible)));
    sqlx::query("UPDATE allocations SET maintenance_lease_until=NULL WHERE id=$1")
        .bind(f.allocation.uuid())
        .execute(&pool)
        .await
        .unwrap();
    assert!(prepare(&f).await.unwrap().is_some());
}
#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn concurrent_preparations_share_one_durable_scope(pool: PgPool) {
    let f = Fixture::new(&pool).await;
    released(&f).await;
    let mut tasks = tokio::task::JoinSet::new();
    for _ in 0..8 {
        let store = f.store.clone();
        let allocation = f.allocation;
        let host = f.host;
        tasks.spawn(async move {
            store
                .prepare_allocation_retirement(allocation, host, 1, 30, false)
                .await
                .unwrap()
        });
    }
    let mut claimed = 0;
    while let Some(result) = tasks.join_next().await {
        claimed += usize::from(result.unwrap().is_some());
    }
    assert_eq!(claimed, 1);
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM allocation_retirements")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 1);
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn active_history_and_output_consumers_prevent_preparation(pool: PgPool) {
    let f = Fixture::new(&pool).await;
    let id = f.admit().await;
    super::history::finish(&f, id).await;
    released(&f).await;
    let p = f
        .store
        .claim_released_history(f.host, 1, Domain::Commands, 30, false)
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(prepare(&f).await, Err(Error::Ineligible)));
    let observed = sandbox_protocol::supervisor::ReleasedHistoryObservation {
        completed_through: p.request.through.clone(),
        request: Some(p.request),
        release_state: sandbox_protocol::supervisor::AllocationState::Released as i32,
        simulated: false,
        observed_unix_ms: now_ms(),
    };
    f.store
        .complete_released_history(&p.claim, &observed, false)
        .await
        .unwrap();
    // Even a terminal operation and acknowledged history cannot bypass an
    // outstanding output-worker claim or a still-retained output consumer.
    sqlx::query("UPDATE operations SET output_status='pending',output_claim_revision=1,output_lease_expires_at=clock_timestamp()+interval '30 seconds' WHERE id=$1").bind(id.uuid()).execute(&pool).await.unwrap();
    assert!(matches!(prepare(&f).await, Err(Error::Ineligible)));
    sqlx::query("UPDATE operations SET output_lease_expires_at=NULL WHERE id=$1")
        .bind(id.uuid())
        .execute(&pool)
        .await
        .unwrap();
    assert!(prepare(&f).await.is_err());
    sqlx::query("UPDATE operations SET output_status='none' WHERE id=$1")
        .bind(id.uuid())
        .execute(&pool)
        .await
        .unwrap();
    assert!(prepare(&f).await.unwrap().is_some());
}
#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn freeze_blocks_new_admission_even_if_outer_state_is_restored(pool: PgPool) {
    let mut f = Fixture::new(&pool).await;
    released(&f).await;
    prepare(&f).await.unwrap().unwrap();
    // Deliberately restore the outer admission predicates: the independent
    // allocation-lock guard must still reject a new command, atomically.
    sqlx::query("UPDATE allocations SET status='running',release_evidence=NULL,released_at=NULL,lease_expires_at=clock_timestamp()+interval '60 seconds' WHERE id=$1").bind(f.allocation.uuid()).execute(&pool).await.unwrap();
    sqlx::query("UPDATE sandboxes SET desired_state='running',observed_state='running',current_allocation_id=$2 WHERE id=$1").bind(f.request.sandbox_id.uuid()).bind(f.allocation.uuid()).execute(&pool).await.unwrap();
    f.request.idempotency_key = key();
    assert!(matches!(
        f.store.admit_execute(&f.request).await,
        Err(DispatchError::Conflict)
    ));
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM operations WHERE execution_allocation_id=$1")
            .bind(f.allocation.uuid())
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(count, 0);
}
#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn malformed_acknowledgement_is_not_consumer_closure(pool: PgPool) {
    let f = Fixture::new(&pool).await;
    let id = f.admit().await;
    super::history::finish(&f, id).await;
    released(&f).await;
    history_done(&f).await;
    sqlx::query("UPDATE released_allocation_history SET completion=jsonb_set(completion,'{release_state}','1') WHERE allocation_id=$1").bind(f.allocation.uuid()).execute(&pool).await.unwrap();
    assert!(matches!(prepare(&f).await, Err(Error::Ineligible)));
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM allocation_retirements")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
}
#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn simulation_policy_is_frozen_and_cannot_be_upgraded(pool: PgPool) {
    let f = Fixture::new(&pool).await;
    released(&f).await;
    let p = f
        .store
        .prepare_allocation_retirement(f.allocation, f.host, 1, 30, true)
        .await
        .unwrap()
        .unwrap();
    assert!(p.intent.simulated);
    expire(&f).await;
    assert!(matches!(prepare(&f).await, Err(Error::Evidence)));
    let retry = f
        .store
        .prepare_allocation_retirement(f.allocation, f.host, 1, 30, true)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(retry.intent, p.intent);
}

#[path = "allocation_retirement_completion.rs"]
mod completion;

#[path = "allocation_forgetting.rs"]
mod forgetting;

#[path = "allocation_retirement_scheduling.rs"]
mod scheduling;
