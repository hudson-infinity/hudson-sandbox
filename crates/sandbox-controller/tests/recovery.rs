//! PostgreSQL and simulator recovery checks. Release observations here are synthetic.
#![allow(clippy::unwrap_used, clippy::expect_used)]
mod common;
use common::Fixture;
use sandbox_protocol::{
    Id, OperationId,
    supervisor::{
        AllocationState, Observation, PreviousAllocationObservation, PreviousAllocationRequest,
        supervisor_server::Supervisor,
    },
};
use sandbox_store::{
    claims::{Claim, OperationKind},
    dispatch::{CreateRejection, DispatchError},
    leases::LeaseAction,
    recovery::PreviousAction,
};
use serde_json::{Value, json};
use sqlx::{PgPool, Row};
use std::time::Duration;

async fn advance(f: &Fixture) {
    sqlx::query("UPDATE hosts SET supervisor_epoch=2")
        .execute(f.store.pool())
        .await
        .unwrap();
    sqlx::query("UPDATE allocations SET maintenance_next_at=NULL,maintenance_lease_until=NULL")
        .execute(f.store.pool())
        .await
        .unwrap();
    f.reclaim_now().await;
}
async fn claim(f: &Fixture, kind: OperationKind) -> Claim {
    f.store.claim_next(kind, 30).await.unwrap().unwrap()
}
async fn request(f: &Fixture, claim: &Claim) -> PreviousAllocationRequest {
    let Some(PreviousAction::Reconcile(r)) = f
        .store
        .prepare_previous_allocation(claim, f.config.host, 2)
        .await
        .unwrap()
    else {
        panic!("expected recovery")
    };
    r
}
fn observation(r: &PreviousAllocationRequest) -> PreviousAllocationObservation {
    PreviousAllocationObservation {
        request: Some(r.clone()),
        release: Some(Observation {
            ownership: r.ownership.clone(),
            state: AllocationState::Released as i32,
            simulated: true,
            start_count: 1,
            create_operation_id: r.ownership.as_ref().unwrap().operation_id.clone(),
            observed_unix_ms: (time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000)
                as i64,
            reason: "synthetic verified cleanup".into(),
        }),
    }
}
async fn retained(f: &Fixture) -> bool {
    sqlx::query_scalar("SELECT released_at IS NULL FROM allocations ORDER BY created_at LIMIT 1")
        .fetch_one(f.store.pool())
        .await
        .unwrap()
}
async fn op(f: &Fixture, id: OperationId) -> Value {
    sqlx::query_scalar("SELECT to_jsonb(o) FROM operations o WHERE id=$1")
        .bind(id.uuid())
        .fetch_one(f.store.pool())
        .await
        .unwrap()
}
async fn dispatched(pool: &PgPool) -> (Fixture, Claim, PreviousAllocationRequest) {
    let f = Fixture::new(pool).await;
    f.admit().await;
    let (old, _) = f.prepared().await;
    f.store.record_create_unknown(&old).await.unwrap();
    advance(&f).await;
    let c = claim(&f, OperationKind::Create).await;
    let r = request(&f, &c).await;
    (f, c, r)
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn undispatched_old_reservation_rejects_without_inventing_host_evidence(pool: PgPool) {
    let f = Fixture::new(&pool).await;
    f.admit().await;
    f.store
        .observe_configured_host(f.config.host, 1)
        .await
        .unwrap();
    let old = claim(&f, OperationKind::Create).await;
    f.store
        .reserve_create(&old, f.config.host, 1)
        .await
        .unwrap();
    advance(&f).await;
    let c = claim(&f, OperationKind::Create).await;
    assert!(matches!(
        f.store
            .prepare_previous_allocation(&c, f.config.host, 2)
            .await
            .unwrap(),
        Some(PreviousAction::RejectUndispatched)
    ));
    f.store
        .reject_undispatched_create(&c, CreateRejection::HostEpochChanged)
        .await
        .unwrap();
    assert!(!retained(&f).await);
    let row = op(&f, c.operation_id).await;
    assert_eq!(row["attempt_count"], 0);
    assert_eq!(row["status"], "failed");
    assert_eq!(f.fake.total_starts().await, 0);
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn previous_create_requires_fresh_exact_release_from_current_epoch(pool: PgPool) {
    let (f, c, r) = dispatched(&pool).await;
    let valid = observation(&r);
    let before = op(&f, c.operation_id).await;
    for field in [
        "reporter",
        "original_epoch",
        "generation",
        "project",
        "request",
        "claim",
        "clock",
        "ready",
        "absent",
        "create",
    ] {
        let mut bad = valid.clone();
        match field {
            "reporter" => bad.request.as_mut().unwrap().reporting_epoch = 3,
            "original_epoch" => {
                bad.release
                    .as_mut()
                    .unwrap()
                    .ownership
                    .as_mut()
                    .unwrap()
                    .supervisor_epoch = 2
            }
            "generation" => {
                bad.release
                    .as_mut()
                    .unwrap()
                    .ownership
                    .as_mut()
                    .unwrap()
                    .generation += 1
            }
            "project" => {
                bad.release
                    .as_mut()
                    .unwrap()
                    .ownership
                    .as_mut()
                    .unwrap()
                    .project_id = "wrong".into()
            }
            "request" => {
                bad.request
                    .as_mut()
                    .unwrap()
                    .ownership
                    .as_mut()
                    .unwrap()
                    .allocation_id = "wrong".into()
            }
            "claim" => {
                bad.release
                    .as_mut()
                    .unwrap()
                    .ownership
                    .as_mut()
                    .unwrap()
                    .claim_revision += 1
            }
            "clock" => bad.release.as_mut().unwrap().observed_unix_ms = 1,
            "ready" => bad.release.as_mut().unwrap().state = AllocationState::Ready as i32,
            "absent" => bad.release.as_mut().unwrap().state = AllocationState::Absent as i32,
            _ => {
                bad.release.as_mut().unwrap().create_operation_id =
                    OperationId::generate().to_string()
            }
        }
        assert!(
            f.store
                .record_previous_release(&c, &bad, true)
                .await
                .is_err(),
            "{field}"
        );
        assert!(retained(&f).await);
        assert_eq!(op(&f, c.operation_id).await, before);
    }
    assert!(matches!(
        f.store.record_previous_release(&c, &valid, false).await,
        Err(DispatchError::SimulationDenied)
    ));
    // Ordinary same-epoch release acceptance has not been weakened.
    assert!(matches!(
        f.store
            .record_create_observation(&c, valid.release.as_ref().unwrap(), true)
            .await,
        Err(DispatchError::Conflict)
    ));
    f.store
        .record_previous_release(&c, &valid, true)
        .await
        .unwrap();
    assert!(!retained(&f).await);
    let row = op(&f, c.operation_id).await;
    assert_eq!(row["status"], "failed");
    assert_eq!(row["error"]["create_outcome_unknown"], true);
    assert_eq!(row["attempt_count"], 1);
    assert!(matches!(
        f.store.record_previous_release(&c, &valid, true).await,
        Err(DispatchError::LostClaim)
    ));
    let receipt: Value = sqlx::query_scalar("SELECT release_evidence FROM allocations")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(receipt["supervisor_epoch"], 1);
    assert_eq!(receipt["reporting_epoch"], 2);
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn missing_prior_journal_keeps_uncertainty_and_stale_claims_cannot_release(pool: PgPool) {
    let (f, old, r) = dispatched(&pool).await;
    assert!(
        f.fake
            .reconcile_previous_allocation(tonic::Request::new(r.clone()))
            .await
            .is_err()
    );
    f.store.record_previous_unknown(&old, &r).await.unwrap();
    assert!(retained(&f).await);
    assert_eq!(op(&f, old.operation_id).await["status"], "unknown");
    f.reclaim_now().await;
    let next = claim(&f, OperationKind::Create).await;
    let next_request = request(&f, &next).await;
    assert_eq!(
        r.ownership.as_ref().unwrap().allocation_id,
        next_request.ownership.as_ref().unwrap().allocation_id
    );
    assert!(matches!(
        f.store
            .record_previous_release(&old, &observation(&r), true)
            .await,
        Err(DispatchError::LostClaim)
    ));
    let mut fenced = observation(&next_request);
    let release = fenced.release.as_mut().unwrap();
    release.state = AllocationState::FencedAbsent as i32;
    release.start_count = 0;
    release.create_operation_id.clear();
    f.store
        .record_previous_release(&next, &fenced, true)
        .await
        .unwrap();
    assert!(!retained(&f).await);
    assert_eq!(op(&f, next.operation_id).await["attempt_count"], 1);
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn running_old_owner_gets_cleanup_and_only_verified_release_reopens_capacity(pool: PgPool) {
    let f = Fixture::new(&pool).await;
    sqlx::query("UPDATE hosts SET cpu_capacity=2")
        .execute(&pool)
        .await
        .unwrap();
    let (create, _) = f.admit().await;
    f.controller().await.tick().await.unwrap();
    let before = op(&f, create).await;
    // Seed an unresolved command to prove cleanup cannot manufacture its outcome.
    let command = OperationId::generate();
    sqlx::query("INSERT INTO operations(id,project_id,sandbox_id,kind,initiator_kind,idempotency_key,request_digest,digest_version,payload,status,execution_allocation_id)
        SELECT $1,o.project_id,o.sandbox_id,'execute','service',$2,o.request_digest,1,'{}','unknown',s.current_allocation_id FROM operations o JOIN sandboxes s ON s.id=o.sandbox_id WHERE o.id=$3")
        .bind(command.uuid()).bind(command.to_string()).bind(create.uuid()).execute(&pool).await.unwrap();
    advance(&f).await;
    let command_before = op(&f, command).await;
    assert!(
        f.store
            .claim_allocation(f.config.host, 1, 30)
            .await
            .unwrap()
            .is_none()
    );
    let maintenance = f
        .store
        .claim_allocation(f.config.host, 2, 30)
        .await
        .unwrap()
        .unwrap();
    let LeaseAction::Cleanup(destroy) = f.store.prepare_lease(&maintenance).await.unwrap() else {
        panic!("service cleanup")
    };
    assert!(retained(&f).await);
    assert_eq!(op(&f, create).await, before);
    // A new sandbox still cannot consume the reserved host capacity.
    let (pending, _) = f.admit().await;
    let pending_claim = claim(&f, OperationKind::Create).await;
    assert_eq!(pending_claim.operation_id, pending);
    f.store
        .observe_configured_host(f.config.host, 2)
        .await
        .unwrap();
    assert!(matches!(
        f.store
            .reserve_create(&pending_claim, f.config.host, 2)
            .await,
        Err(sandbox_store::placement::PlacementError::Capacity)
    ));
    let c = claim(&f, OperationKind::Destroy).await;
    assert_eq!(c.operation_id, destroy);
    let r = request(&f, &c).await;
    let valid = observation(&r);
    f.store
        .record_previous_release(&c, &valid, true)
        .await
        .unwrap();
    assert_eq!(op(&f, create).await, before);
    assert_eq!(op(&f, command).await, command_before);
    assert_eq!(op(&f, destroy).await["status"], "succeeded");
    f.store
        .reserve_create(&pending_claim, f.config.host, 2)
        .await
        .unwrap();
    let epochs: Vec<i64> =
        sqlx::query_scalar("SELECT supervisor_epoch FROM allocations ORDER BY created_at")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(epochs, vec![1, 2]);
    assert_eq!(f.fake.total_starts().await, 1);
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn superseding_destroy_preserves_unknown_create_until_verified_cleanup(pool: PgPool) {
    let (f, c, r) = dispatched(&pool).await;
    f.store.record_previous_unknown(&c, &r).await.unwrap();
    let sandbox = &r.ownership.as_ref().unwrap().sandbox_id;
    let (status, body) = f
        .send(
            "POST",
            &format!("/v1/sandboxes/{sandbox}/destroy"),
            json!({}),
        )
        .await;
    assert_eq!(status, http::StatusCode::ACCEPTED);
    let destroy: OperationId = body["operation_id"].as_str().unwrap().parse().unwrap();
    assert_eq!(op(&f, c.operation_id).await["status"], "unknown");
    let d = claim(&f, OperationKind::Destroy).await;
    let r = request(&f, &d).await;
    f.store
        .record_previous_release(&d, &observation(&r), true)
        .await
        .unwrap();
    assert_eq!(op(&f, c.operation_id).await["status"], "failed");
    assert_eq!(
        op(&f, c.operation_id).await["error"]["create_outcome_unknown"],
        true
    );
    assert_eq!(op(&f, destroy).await["status"], "succeeded");
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn lock_wait_expiry_cancellation_and_new_reporting_epoch_preserve_reservation(pool: PgPool) {
    let (f, c, _) = dispatched(&pool).await;
    // Prepare an exact request after shortening the retained operation claim.
    sqlx::query("UPDATE operations SET lease_expires_at=clock_timestamp()+interval '300 milliseconds' WHERE id=$1").bind(c.operation_id.uuid()).execute(&pool).await.unwrap();
    let r = request(&f, &c).await;
    let mut held = pool.begin().await.unwrap();
    sqlx::query("SELECT * FROM hosts FOR UPDATE")
        .execute(&mut *held)
        .await
        .unwrap();
    let store = f.store.clone();
    let cc = c.clone();
    let o = observation(&r);
    let job = tokio::spawn(async move { store.record_previous_release(&cc, &o, true).await });
    tokio::time::sleep(Duration::from_millis(500)).await;
    held.commit().await.unwrap();
    assert!(matches!(job.await.unwrap(), Err(DispatchError::LostClaim)));
    assert!(retained(&f).await);
    f.reclaim_now().await;
    let c = claim(&f, OperationKind::Create).await;
    let r = request(&f, &c).await;
    let mut held = pool.begin().await.unwrap();
    sqlx::query("SELECT * FROM hosts FOR UPDATE")
        .execute(&mut *held)
        .await
        .unwrap();
    let store = f.store.clone();
    let cc = c.clone();
    let o = observation(&r);
    let job = tokio::spawn(async move { store.record_previous_release(&cc, &o, true).await });
    tokio::time::sleep(Duration::from_millis(100)).await;
    job.abort();
    assert!(job.await.unwrap_err().is_cancelled());
    held.commit().await.unwrap();
    assert!(retained(&f).await);
    sqlx::query("UPDATE hosts SET supervisor_epoch=3")
        .execute(&pool)
        .await
        .unwrap();
    assert!(matches!(
        f.store
            .record_previous_release(&c, &observation(&r), true)
            .await,
        Err(DispatchError::Conflict)
    ));
    assert!(retained(&f).await);
    let row = sqlx::query("SELECT status,supervisor_epoch FROM allocations")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(row.get::<i64, _>("supervisor_epoch"), 1);
    assert_ne!(row.get::<String, _>("status"), "released");
}
