//! Durable maintenance and failure recovery through PostgreSQL and real mTLS.
#![allow(clippy::unwrap_used, clippy::expect_used)]
mod common;
use common::Fixture;
use sandbox_controller::Tick;
use sandbox_protocol::{
    Id,
    supervisor::{AllocationState, LeaseObservation, LeaseRequest, supervisor_server::Supervisor},
};
use sandbox_store::{
    dispatch::DispatchError,
    leases::{AllocationClaim, LeaseAction, LeaseResult},
};
use serde_json::{Value, json};
use sqlx::PgPool;
use tonic::Request;

async fn due(f: &Fixture) {
    sqlx::query("UPDATE allocations SET maintenance_next_at=NULL,maintenance_lease_until=NULL WHERE released_at IS NULL").execute(f.store.pool()).await.unwrap();
}
async fn ready(pool: &PgPool) -> Fixture {
    let f = Fixture::new(pool).await;
    f.admit().await;
    assert_eq!(f.controller().await.tick().await.unwrap(), Tick::Confirmed);
    due(&f).await;
    f
}
async fn claim(f: &Fixture, seconds: u32) -> AllocationClaim {
    f.store
        .claim_allocation(f.config.host, 1, seconds)
        .await
        .unwrap()
        .unwrap()
}
async fn prepared(f: &Fixture) -> (AllocationClaim, LeaseRequest) {
    let claim = claim(f, 30).await;
    let LeaseAction::Renew(request) = f.store.prepare_lease(&claim).await.unwrap() else {
        panic!("renew")
    };
    (claim, request)
}
async fn observe(f: &Fixture, request: LeaseRequest) -> LeaseObservation {
    f.fake
        .renew_lease(Request::new(request))
        .await
        .unwrap()
        .into_inner()
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn running_allocation_renews_without_reopening_create(pool: PgPool) {
    let f = ready(&pool).await;
    assert_eq!(f.controller().await.tick().await.unwrap(), Tick::Maintained);
    let (after, now, pending, source): (i64, i64, bool, Value) = sqlx::query_as(
        "SELECT floor(extract(epoch FROM lease_expires_at)*1000)::bigint,
        floor(extract(epoch FROM clock_timestamp())*1000)::bigint,renewal_pending,lease_observation FROM allocations",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(
        after > now,
        "renewed execution lease must remain in the future"
    );
    assert!(
        after <= now + 30_000,
        "renewed execution lease must stay within the 30-second renewal window"
    );
    assert!(!pending);
    assert_eq!(source["simulated"], true);
    let (status, count): (String, i64) =
        sqlx::query_as("SELECT min(status),count(*) FROM operations")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(status, "succeeded");
    assert_eq!(count, 1);
    assert_eq!(f.fake.total_starts().await, 1);
    assert_eq!(f.controller().await.tick().await.unwrap(), Tick::Idle);
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn lost_renewal_reply_is_unknown_until_inspected(pool: PgPool) {
    let f = ready(&pool).await;
    let (before,): (String,) = sqlx::query_as("SELECT lease_expires_at::text FROM allocations")
        .fetch_one(&pool)
        .await
        .unwrap();
    f.fake.lose_next_renew_reply().await;
    assert_eq!(f.controller().await.tick().await.unwrap(), Tick::Unknown);
    let (after, pending, release): (String, bool, bool) = sqlx::query_as(
        "SELECT lease_expires_at::text,renewal_pending,released_at IS NOT NULL FROM allocations",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(before, after);
    assert!(pending);
    assert!(!release);
    let (state,): (String,) = sqlx::query_as("SELECT observed_state FROM sandboxes")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(state, "unknown");
    due(&f).await;
    assert_eq!(f.controller().await.tick().await.unwrap(), Tick::Maintained);
    let (state, source): (String, bool) =
        sqlx::query_as("SELECT observed_state,observation_simulated FROM sandboxes")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(state, "running");
    assert!(source);
    assert_eq!(f.fake.total_starts().await, 1);
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn crash_after_intent_inspects_before_another_extension(pool: PgPool) {
    let f = ready(&pool).await;
    let (old, _) = prepared(&f).await;
    due(&f).await;
    let next = claim(&f, 30).await;
    assert!(next.revision > old.revision);
    assert!(matches!(
        f.store.prepare_lease(&next).await.unwrap(),
        LeaseAction::Inspect(_)
    ));
    assert!(matches!(
        f.store.prepare_lease(&old).await,
        Err(DispatchError::LostClaim)
    ));
    due(&f).await;
    assert_eq!(f.controller().await.tick().await.unwrap(), Tick::Maintained);
    assert_eq!(f.fake.total_starts().await, 1);
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn concurrent_claims_have_one_owner_and_stale_responses_cannot_commit(pool: PgPool) {
    let f = ready(&pool).await;
    let mut tasks = tokio::task::JoinSet::new();
    for _ in 0..8 {
        let store = f.store.clone();
        let host = f.config.host;
        tasks.spawn(async move { store.claim_allocation(host, 1, 30).await.unwrap() });
    }
    let mut owners = Vec::new();
    while let Some(result) = tasks.join_next().await {
        if let Some(c) = result.unwrap() {
            owners.push(c);
        }
    }
    assert_eq!(owners.len(), 1);
    let old = owners.pop().unwrap();
    let LeaseAction::Renew(request) = f.store.prepare_lease(&old).await.unwrap() else {
        panic!("renew")
    };
    let observed = observe(&f, request).await;
    due(&f).await;
    let current = claim(&f, 30).await;
    assert!(current.revision > old.revision);
    assert!(matches!(
        f.store
            .record_lease_observation(&old, &observed, true)
            .await,
        Err(DispatchError::LostClaim)
    ));
    let (empty,): (bool,) = sqlx::query_as("SELECT lease_observation IS NULL FROM allocations")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(empty);
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn invalid_lease_evidence_never_releases_or_extends_capacity(pool: PgPool) {
    let f = ready(&pool).await;
    let (claim, request) = prepared(&f).await;
    let observation = observe(&f, request).await;
    for field in [
        "revision",
        "epoch",
        "generation",
        "allocation",
        "deadline",
        "clock",
        "absence",
    ] {
        let mut bad = observation.clone();
        match field {
            "revision" => bad.ownership.as_mut().unwrap().revision += 1,
            "epoch" => bad.ownership.as_mut().unwrap().supervisor_epoch += 1,
            "generation" => bad.ownership.as_mut().unwrap().generation += 1,
            "allocation" => bad.ownership.as_mut().unwrap().allocation_id = "bad".into(),
            "deadline" => bad.allocation_expires_unix_ms += 1000,
            "clock" => bad.observed_unix_ms = 1,
            _ => bad.state = AllocationState::Absent as i32,
        }
        assert!(
            matches!(
                f.store.record_lease_observation(&claim, &bad, true).await,
                Err(DispatchError::BadEvidence)
            ),
            "{field}"
        );
    }
    assert!(matches!(
        f.store
            .record_lease_observation(&claim, &observation, false)
            .await,
        Err(DispatchError::SimulationDenied)
    ));
    let (held, empty): (bool, bool) =
        sqlx::query_as("SELECT released_at IS NULL,lease_observation IS NULL FROM allocations")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(held && empty);
    assert_eq!(
        f.store
            .record_lease_observation(&claim, &observation, true)
            .await
            .unwrap(),
        LeaseResult::Renewed
    );
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn suspension_queues_ordinary_service_destroy_and_keeps_create_history(pool: PgPool) {
    let f = ready(&pool).await;
    sqlx::query("UPDATE projects SET status='suspended',api_tokens='[]'")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(f.controller().await.tick().await.unwrap(), Tick::Confirmed);
    let (state,held):(String,i64)=sqlx::query_as("SELECT min(observed_state),(SELECT count(*) FROM allocations WHERE released_at IS NULL) FROM sandboxes").fetch_one(&pool).await.unwrap();
    assert_eq!(state, "destroyed");
    assert_eq!(held, 0);
    let (initiator, status): (String, String) =
        sqlx::query_as("SELECT initiator_kind,status FROM operations WHERE kind='destroy'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(initiator, "service");
    assert_eq!(status, "succeeded");
    let (status,): (String,) = sqlx::query_as("SELECT status FROM operations WHERE kind='create'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(status, "succeeded");
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn delayed_renewal_cannot_undo_destroy(pool: PgPool) {
    let f = ready(&pool).await;
    let (claim, request) = prepared(&f).await;
    let (sandbox,): (uuid::Uuid,) = sqlx::query_as("SELECT id FROM sandboxes")
        .fetch_one(&pool)
        .await
        .unwrap();
    let path = format!(
        "/v1/sandboxes/{}/destroy",
        sandbox_protocol::SandboxId::from_uuid(sandbox)
    );
    assert_eq!(
        f.send("POST", &path, json!({})).await.0,
        http::StatusCode::ACCEPTED
    );
    assert_eq!(f.controller().await.tick().await.unwrap(), Tick::Confirmed);
    let observed = observe(&f, request).await;
    assert_eq!(observed.state, AllocationState::Released as i32);
    assert!(matches!(
        f.store
            .record_lease_observation(&claim, &observed, true)
            .await,
        Err(DispatchError::LostClaim | DispatchError::Conflict)
    ));
    let (state,): (String,) = sqlx::query_as("SELECT observed_state FROM sandboxes")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(state, "destroyed");
    assert_eq!(f.fake.total_starts().await, 1);
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn watchdog_release_reconciles_through_destroy_after_expiry(pool: PgPool) {
    let f = Fixture::new(&pool).await;
    f.admit().await;
    let (create, mut request) = f.prepared().await;
    request.allocation_expires_unix_ms = sandbox_fake_host::unix_ms().unwrap() + 3000;
    sqlx::query("UPDATE allocations SET lease_expires_at=to_timestamp($1::double precision/1000)")
        .bind(request.allocation_expires_unix_ms as f64)
        .execute(&pool)
        .await
        .unwrap();
    let observation = f
        .fake
        .create(Request::new(request))
        .await
        .unwrap()
        .into_inner();
    f.store
        .record_create_observation(&create, &observation, true)
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(3100)).await;
    f.fake.expire_leases().await;
    due(&f).await;
    assert_eq!(f.controller().await.tick().await.unwrap(), Tick::Confirmed);
    let (state,held):(String,i64)=sqlx::query_as("SELECT min(observed_state),(SELECT count(*) FROM allocations WHERE released_at IS NULL) FROM sandboxes").fetch_one(&pool).await.unwrap();
    assert_eq!(state, "destroyed");
    assert_eq!(held, 0);
    assert_eq!(f.fake.total_starts().await, 1);
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn shortened_sandbox_deadline_stops_instead_of_extending(pool: PgPool) {
    let f = ready(&pool).await;
    sqlx::query("UPDATE sandboxes SET expires_at=clock_timestamp()+interval '1 second'")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(f.controller().await.tick().await.unwrap(), Tick::Confirmed);
    let (state,): (String,) = sqlx::query_as("SELECT observed_state FROM sandboxes")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(state, "destroyed");
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn maintenance_runs_alongside_create_and_destroy_queues(pool: PgPool) {
    let f = ready(&pool).await;
    let (_, second) = f.admit().await;
    let mut controller = f.controller().await;
    assert_eq!(controller.tick().await.unwrap(), Tick::Confirmed);
    let (maintained,): (i64,) =
        sqlx::query_as("SELECT count(*) FROM allocations WHERE lease_observation IS NOT NULL")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(maintained, 1);
    f.send(
        "POST",
        &format!("/v1/sandboxes/{second}/destroy"),
        json!({}),
    )
    .await;
    // Both kinds queued: alternate preference must still make progress.
    f.admit().await;
    let first = controller.tick().await.unwrap();
    let second = controller.tick().await.unwrap();
    assert!(matches!(first, Tick::Confirmed | Tick::Deferred));
    assert_eq!(second, Tick::Confirmed);
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn an_uncertain_create_does_not_block_another_allocations_renewal(pool: PgPool) {
    let f = ready(&pool).await;
    f.admit().await;
    let (_old_create, _unsent) = f.prepared().await;
    f.reclaim_now().await;
    assert_eq!(f.controller().await.tick().await.unwrap(), Tick::Unknown);
    let (maintained,): (i64,) =
        sqlx::query_as("SELECT count(*) FROM allocations WHERE lease_observation IS NOT NULL")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(maintained, 1);
    assert_eq!(f.fake.total_starts().await, 1);
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn lease_expiry_while_waiting_for_project_lock_rolls_back_the_observation(pool: PgPool) {
    let f = ready(&pool).await;
    let claim = claim(&f, 1).await;
    let LeaseAction::Renew(request) = f.store.prepare_lease(&claim).await.unwrap() else {
        panic!("renew")
    };
    let observation = observe(&f, request).await;
    let mut blocker = pool.begin().await.unwrap();
    sqlx::query("SELECT id FROM projects FOR UPDATE")
        .fetch_one(&mut *blocker)
        .await
        .unwrap();
    let store = f.store.clone();
    let old = claim.clone();
    let waiter = tokio::spawn(async move {
        store
            .record_lease_observation(&old, &observation, true)
            .await
    });
    tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
    blocker.commit().await.unwrap();
    assert!(matches!(
        waiter.await.unwrap(),
        Err(DispatchError::LostClaim)
    ));
    let (empty, pending): (bool, bool) =
        sqlx::query_as("SELECT lease_observation IS NULL,renewal_pending FROM allocations")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(empty && pending);
    let replacement = f
        .store
        .claim_allocation(f.config.host, 1, 30)
        .await
        .unwrap()
        .unwrap();
    assert!(replacement.revision > claim.revision);
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn health_failure_marks_running_observation_unknown_without_freeing_capacity(pool: PgPool) {
    let f = ready(&pool).await;
    let mut controller = f.controller().await;
    f.fake.fail_next_health_check().await;
    assert!(controller.tick().await.is_err());
    let (state,held):(String,i64)=sqlx::query_as("SELECT min(observed_state),(SELECT count(*) FROM allocations WHERE released_at IS NULL) FROM sandboxes").fetch_one(&pool).await.unwrap();
    assert_eq!(state, "unknown");
    assert_eq!(held, 1);
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn credential_rotation_does_not_cancel_already_admitted_execution(pool: PgPool) {
    let f = ready(&pool).await;
    sqlx::query("UPDATE projects SET api_tokens='[]'")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(f.controller().await.tick().await.unwrap(), Tick::Maintained);
    let (state,): (String,) = sqlx::query_as("SELECT observed_state FROM sandboxes")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(state, "running");
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn initial_and_renewed_execution_leases_are_capped_by_sandbox_deadline(pool: PgPool) {
    let f = Fixture::new(&pool).await;
    f.admit().await;
    sqlx::query("UPDATE sandboxes SET expires_at=clock_timestamp()+interval '20 seconds'")
        .execute(&pool)
        .await
        .unwrap();
    let (claim, request) = f.prepared().await;
    let (deadline,): (i64,) =
        sqlx::query_as("SELECT floor(extract(epoch FROM expires_at)*1000)::bigint FROM sandboxes")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(request.allocation_expires_unix_ms, deadline);
    let observation = f
        .fake
        .create(Request::new(request))
        .await
        .unwrap()
        .into_inner();
    f.store
        .record_create_observation(&claim, &observation, true)
        .await
        .unwrap();
    due(&f).await;
    assert_eq!(f.controller().await.tick().await.unwrap(), Tick::Maintained);
    let (lease,): (i64,) = sqlx::query_as(
        "SELECT floor(extract(epoch FROM lease_expires_at)*1000)::bigint FROM allocations",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(lease, deadline);
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn replacement_epoch_cannot_confirm_or_release_old_allocations(pool: PgPool) {
    let f = ready(&pool).await;
    let (claim, request) = prepared(&f).await;
    let observation = observe(&f, request).await;
    sqlx::query("UPDATE hosts SET supervisor_epoch=2")
        .execute(&pool)
        .await
        .unwrap();
    assert!(matches!(
        f.store
            .record_lease_observation(&claim, &observation, true)
            .await,
        Err(DispatchError::Conflict)
    ));
    f.store
        .observe_configured_host(f.config.host, 2)
        .await
        .unwrap();
    assert!(
        f.store
            .claim_allocation(f.config.host, 2, 10)
            .await
            .unwrap()
            .is_none()
    );
    let (state,held):(String,i64)=sqlx::query_as("SELECT min(observed_state),(SELECT count(*) FROM allocations WHERE released_at IS NULL) FROM sandboxes").fetch_one(&pool).await.unwrap();
    assert_eq!(state, "unknown");
    assert_eq!(held, 1);
    assert_eq!(f.fake.total_starts().await, 1);
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn suspension_between_renewal_and_acknowledgement_admits_cleanup(pool: PgPool) {
    let f = ready(&pool).await;
    let (claim, request) = prepared(&f).await;
    let observation = observe(&f, request).await;
    sqlx::query("UPDATE projects SET status='suspended'")
        .execute(&pool)
        .await
        .unwrap();
    assert!(matches!(
        f.store
            .record_lease_observation(&claim, &observation, true)
            .await
            .unwrap(),
        LeaseResult::Cleanup(_)
    ));
    let (state,held):(String,i64)=sqlx::query_as("SELECT min(observed_state),(SELECT count(*) FROM allocations WHERE released_at IS NULL) FROM sandboxes").fetch_one(&pool).await.unwrap();
    assert_eq!(state, "destroying");
    assert_eq!(held, 1);
    assert_eq!(f.controller().await.tick().await.unwrap(), Tick::Confirmed);
    let (held,): (i64,) =
        sqlx::query_as("SELECT count(*) FROM allocations WHERE released_at IS NULL")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(held, 0);
}
