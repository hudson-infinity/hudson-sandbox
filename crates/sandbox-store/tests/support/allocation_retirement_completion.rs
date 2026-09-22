//! Synthetic authenticated-observation fixtures establish SQL ownership gates,
//! not physical isolation or a successful network handoff.
use super::*;
use sandbox_protocol::{
    allocation_retirement::Request,
    supervisor::{AllocationMetadataObservation, AllocationMetadataRequest},
};
use serde_json::Value;
fn observation(r: &Request) -> AllocationMetadataObservation {
    AllocationMetadataObservation {
        request: Some(AllocationMetadataRequest {
            request_json: r.encode().unwrap(),
        }),
        observed_unix_ms: now_ms(),
    }
}
async fn pending(pool: &PgPool) -> bool {
    sqlx::query_scalar("SELECT metadata_completion IS NULL AND metadata_completed_at IS NULL FROM allocation_retirements")
        .fetch_one(pool).await.unwrap()
}
async fn blocked(pool: &PgPool, pattern: &str) {
    let until = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        let yes: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE datname=current_database() AND query LIKE $1 AND cardinality(pg_blocking_pids(pid))>0)")
            .bind(pattern).fetch_one(pool).await.unwrap();
        if yes {
            return;
        }
        assert!(
            tokio::time::Instant::now() < until,
            "expected SQL lock wait did not occur"
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
}
#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn completion_is_durable_idempotent_and_never_rewrites_original_results(pool: PgPool) {
    let f = Fixture::new(&pool).await;
    released(&f).await;
    let before: Value =
        sqlx::query_scalar("SELECT jsonb_agg(to_jsonb(o) ORDER BY id) FROM operations o")
            .fetch_one(&pool)
            .await
            .unwrap();
    let r = prepare(&f).await.unwrap().unwrap();
    let observed = observation(&r);
    let completion = f
        .store
        .complete_allocation_retirement(&r, &observed)
        .await
        .unwrap();
    assert_eq!(completion.request, r);
    assert_eq!(
        f.store
            .allocation_retirement_completion(&r.intent)
            .await
            .unwrap(),
        Some(completion.clone())
    );
    assert!(prepare(&f).await.unwrap().is_none());
    sqlx::query("UPDATE hosts SET supervisor_epoch=2 WHERE id=$1")
        .bind(f.host.uuid())
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        f.store
            .complete_allocation_retirement(&r, &observed)
            .await
            .unwrap(),
        completion
    );
    assert!(
        f.store
            .prepare_allocation_retirement(f.allocation, f.host, 2, 30, false)
            .await
            .unwrap()
            .is_none()
    );
    let mut changed = observed.clone();
    changed.observed_unix_ms += 1;
    assert!(
        f.store
            .complete_allocation_retirement(&r, &changed)
            .await
            .is_err()
    );
    assert_eq!(
        f.store
            .allocation_retirement_completion(&r.intent)
            .await
            .unwrap(),
        Some(completion)
    );
    let after: Value =
        sqlx::query_scalar("SELECT jsonb_agg(to_jsonb(o) ORDER BY id) FROM operations o")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(before, after);
    let lease: Option<OffsetDateTime> =
        sqlx::query_scalar("SELECT lease_expires_at FROM allocation_retirements")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(lease.is_none());
}
#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn old_claim_and_old_epoch_cannot_consume_a_new_claim(pool: PgPool) {
    let f = Fixture::new(&pool).await;
    released(&f).await;
    let first = prepare(&f).await.unwrap().unwrap();
    expire(&f).await;
    let second = prepare(&f).await.unwrap().unwrap();
    assert!(matches!(
        f.store
            .complete_allocation_retirement(&first, &observation(&first))
            .await,
        Err(Error::LostClaim)
    ));
    sqlx::query("UPDATE hosts SET supervisor_epoch=2 WHERE id=$1")
        .bind(f.host.uuid())
        .execute(&pool)
        .await
        .unwrap();
    assert!(matches!(
        f.store
            .complete_allocation_retirement(&second, &observation(&second))
            .await,
        Err(Error::LostClaim)
    ));
    assert!(pending(&pool).await);
    expire(&f).await;
    let current = f
        .store
        .prepare_allocation_retirement(f.allocation, f.host, 2, 30, false)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(current.intent, first.intent);
    f.store
        .complete_allocation_retirement(&current, &observation(&current))
        .await
        .unwrap();
}
#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn changed_scope_claim_echo_and_observation_clock_never_complete(pool: PgPool) {
    let f = Fixture::new(&pool).await;
    released(&f).await;
    let r = prepare(&f).await.unwrap().unwrap();
    for field in 0..14 {
        let mut changed = r.clone();
        match field {
            0 => changed.intent.permit.host = HostId::generate(),
            1 => changed.intent.permit.project = ProjectId::generate(),
            2 => changed.intent.permit.sandbox = SandboxId::generate(),
            3 => changed.intent.permit.allocation = AllocationId::generate(),
            4 => changed.intent.permit.generation += 1,
            5 => changed.intent.permit.original_epoch += 1,
            6 => changed.intent.permit.serial += 1,
            7 => changed.intent.permit.create_operation = OperationId::generate(),
            8 => changed.intent.retirement = OperationId::generate(),
            9 => {
                changed.intent.commands = DomainClosure::Retired {
                    through: OperationId::generate(),
                }
            }
            10 => {
                changed.intent.files = DomainClosure::Retired {
                    through: OperationId::generate(),
                }
            }
            11 => changed.intent.release_evidence_sha256 = "ef".repeat(32),
            12 => changed.revision += 1,
            _ => changed.expires_unix_ms += 1,
        }
        assert!(
            f.store
                .complete_allocation_retirement(&changed, &observation(&changed))
                .await
                .is_err(),
            "field {field}"
        );
        assert!(pending(&pool).await);
    }
    for time in [0, now_ms() - 11000, now_ms() + 6000, r.expires_unix_ms + 1] {
        let mut o = observation(&r);
        o.observed_unix_ms = time;
        assert!(
            f.store
                .complete_allocation_retirement(&r, &o)
                .await
                .is_err()
        );
    }
    let mut o = observation(&r);
    o.request = None;
    assert!(
        f.store
            .complete_allocation_retirement(&r, &o)
            .await
            .is_err()
    );
    o = observation(&r);
    o.request.as_mut().unwrap().request_json.push(b' ');
    assert!(
        f.store
            .complete_allocation_retirement(&r, &o)
            .await
            .is_err()
    );
    assert!(pending(&pool).await);
    f.store
        .complete_allocation_retirement(&r, &observation(&r))
        .await
        .unwrap();
}
#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn concurrent_identical_acknowledgements_retain_one_completion(pool: PgPool) {
    let f = Fixture::new(&pool).await;
    released(&f).await;
    let r = prepare(&f).await.unwrap().unwrap();
    let o = observation(&r);
    let mut tasks = tokio::task::JoinSet::new();
    for _ in 0..8 {
        let store = f.store.clone();
        let r = r.clone();
        let o = o.clone();
        tasks.spawn(async move { store.complete_allocation_retirement(&r, &o).await.unwrap() });
    }
    let first = tasks.join_next().await.unwrap().unwrap();
    while let Some(result) = tasks.join_next().await {
        assert_eq!(result.unwrap(), first);
    }
    assert!(!pending(&pool).await);
}
#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn lease_expiring_while_waiting_for_allocation_lock_cannot_complete(pool: PgPool) {
    let f = Fixture::new(&pool).await;
    released(&f).await;
    let r = f
        .store
        .prepare_allocation_retirement(f.allocation, f.host, 1, 1, false)
        .await
        .unwrap()
        .unwrap();
    let o = observation(&r);
    let store = f.store.clone();
    let mut lock = pool.begin().await.unwrap();
    sqlx::query("SELECT id FROM allocations WHERE id=$1 FOR UPDATE")
        .bind(f.allocation.uuid())
        .fetch_one(&mut *lock)
        .await
        .unwrap();
    let task = tokio::spawn(async move { store.complete_allocation_retirement(&r, &o).await });
    blocked(&pool, "SELECT * FROM allocations WHERE id=$1 FOR UPDATE").await;
    sqlx::query("SELECT pg_sleep(GREATEST(extract(epoch FROM (lease_expires_at-clock_timestamp())),0)+0.01) FROM allocation_retirements")
        .execute(&mut *lock).await.unwrap();
    lock.rollback().await.unwrap();
    assert!(matches!(task.await.unwrap(), Err(Error::LostClaim)));
    assert!(pending(&pool).await);
}
#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn host_epoch_change_winning_the_lock_rejects_old_acknowledgement(pool: PgPool) {
    let f = Fixture::new(&pool).await;
    released(&f).await;
    let r = prepare(&f).await.unwrap().unwrap();
    let o = observation(&r);
    let store = f.store.clone();
    let mut epoch = pool.begin().await.unwrap();
    sqlx::query("UPDATE hosts SET supervisor_epoch=2 WHERE id=$1")
        .bind(f.host.uuid())
        .execute(&mut *epoch)
        .await
        .unwrap();
    let task = tokio::spawn(async move { store.complete_allocation_retirement(&r, &o).await });
    blocked(&pool, "SELECT id FROM hosts%FOR SHARE").await;
    epoch.commit().await.unwrap();
    assert!(matches!(task.await.unwrap(), Err(Error::LostClaim)));
    assert!(pending(&pool).await);
}
#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn acknowledgement_holds_host_epoch_until_its_commit(pool: PgPool) {
    let f = Fixture::new(&pool).await;
    released(&f).await;
    let r = prepare(&f).await.unwrap().unwrap();
    let o = observation(&r);
    let store = f.store.clone();
    sqlx::raw_sql("CREATE FUNCTION hold_metadata_ack() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.metadata_completion IS NOT NULL THEN PERFORM pg_advisory_xact_lock(314159); END IF; RETURN NEW; END $$; CREATE TRIGGER hold_metadata_ack BEFORE UPDATE ON allocation_retirements FOR EACH ROW EXECUTE FUNCTION hold_metadata_ack();").execute(&pool).await.unwrap();
    let mut gate = pool.begin().await.unwrap();
    sqlx::query("SELECT pg_advisory_xact_lock(314159)")
        .execute(&mut *gate)
        .await
        .unwrap();
    let ack = tokio::spawn(async move { store.complete_allocation_retirement(&r, &o).await });
    blocked(
        &pool,
        "%UPDATE allocation_retirements SET metadata_completion=%",
    )
    .await;
    let epoch_pool = pool.clone();
    let host = f.host;
    let epoch = tokio::spawn(async move {
        sqlx::query("UPDATE hosts SET supervisor_epoch=2 WHERE id=$1")
            .bind(host.uuid())
            .execute(&epoch_pool)
            .await
            .unwrap()
    });
    blocked(&pool, "UPDATE hosts SET supervisor_epoch=2 WHERE id=$1").await;
    gate.rollback().await.unwrap();
    let completed = ack.await.unwrap().unwrap();
    epoch.await.unwrap();
    assert_eq!(
        f.store
            .allocation_retirement_completion(&completed.request.intent)
            .await
            .unwrap(),
        Some(completed)
    );
}
#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn changed_release_or_consumers_after_preparation_prevent_completion(pool: PgPool) {
    let f = Fixture::new(&pool).await;
    let command = f.admit().await;
    super::super::history::finish(&f, command).await;
    released(&f).await;
    history_done(&f).await;
    let r = prepare(&f).await.unwrap().unwrap();
    let release: Value = sqlx::query_scalar("SELECT release_evidence FROM allocations WHERE id=$1")
        .bind(f.allocation.uuid())
        .fetch_one(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE allocations SET release_evidence=jsonb_set(release_evidence,'{observed_unix_ms}','1') WHERE id=$1").bind(f.allocation.uuid()).execute(&pool).await.unwrap();
    assert!(
        f.store
            .complete_allocation_retirement(&r, &observation(&r))
            .await
            .is_err()
    );
    sqlx::query("UPDATE allocations SET release_evidence=$2 WHERE id=$1")
        .bind(f.allocation.uuid())
        .bind(release)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE operations SET output_status='pending' WHERE id=$1")
        .bind(command.uuid())
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        f.store
            .complete_allocation_retirement(&r, &observation(&r))
            .await
            .is_err()
    );
    assert!(pending(&pool).await);
    sqlx::query("UPDATE operations SET output_status='none' WHERE id=$1")
        .bind(command.uuid())
        .execute(&pool)
        .await
        .unwrap();
    f.store
        .complete_allocation_retirement(&r, &observation(&r))
        .await
        .unwrap();
}
#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn simulated_preparation_and_partial_or_changed_sql_completion_are_rejected(pool: PgPool) {
    let f = Fixture::new(&pool).await;
    released(&f).await;
    let r = f
        .store
        .prepare_allocation_retirement(f.allocation, f.host, 1, 30, true)
        .await
        .unwrap()
        .unwrap();
    assert!(
        f.store
            .complete_allocation_retirement(&r, &observation(&r))
            .await
            .is_err()
    );
    let mut physical = r.clone();
    physical.intent.simulated = false;
    assert!(
        f.store
            .complete_allocation_retirement(&physical, &observation(&physical))
            .await
            .is_err()
    );
    assert!(
        sqlx::query("UPDATE allocation_retirements SET metadata_completed_at=clock_timestamp()")
            .execute(&pool)
            .await
            .is_err()
    );
    let e = json!({"version":1,"request":r,"observed_unix_ms":now_ms()});
    assert!(sqlx::query("UPDATE allocation_retirements SET metadata_completion=$1,metadata_completed_at=clock_timestamp(),metadata_claim_expires_at=lease_expires_at,lease_expires_at=NULL")
        .bind(e).execute(&pool).await.is_err());
    assert!(pending(&pool).await);
}
