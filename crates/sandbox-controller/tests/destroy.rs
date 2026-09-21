//! Destroy uses the same live gRPC/TLS fake and real database as create.
#![allow(clippy::unwrap_used, clippy::expect_used)]
mod common;
use axum::body::{Body, to_bytes};
use common::Fixture;
use http::{Request, StatusCode};
use sandbox_controller::Tick;
use sandbox_protocol::{
    Id, SandboxId,
    supervisor::{AllocationState, InspectRequest, StopRequest, supervisor_server::Supervisor},
};
use sandbox_store::{claims::OperationKind, destroy::DestroyAction, dispatch::DispatchError};
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt;

async fn destroy(f: &Fixture, sandbox: SandboxId, key: &str, body: Value) -> (StatusCode, Value) {
    let response = f
        .app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/v1/sandboxes/{sandbox}/destroy"))
                .header("authorization", format!("Bearer {}", f.token))
                .header("content-type", "application/json")
                .header("idempotency-key", key)
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let body = to_bytes(response.into_body(), 65536).await.unwrap();
    (status, serde_json::from_slice(&body).unwrap())
}
async fn running(f: &Fixture) -> SandboxId {
    let (_, sandbox) = f.admit().await;
    assert_eq!(f.controller().await.tick().await.unwrap(), Tick::Confirmed);
    sandbox
}
async fn unreleased(pool: &PgPool) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM allocations WHERE released_at IS NULL")
        .fetch_one(pool)
        .await
        .unwrap()
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn destroy_completes_and_retries_keep_identity_and_tombstone(pool: PgPool) {
    let f = Fixture::new(&pool).await;
    let sandbox = running(&f).await;
    let (status, first) = destroy(&f, sandbox, "destroy-request-01", json!({})).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(first["status"], "queued");
    assert_eq!(unreleased(&pool).await, 1);
    assert_eq!(f.controller().await.tick().await.unwrap(), Tick::Confirmed);
    assert_eq!(unreleased(&pool).await, 0);
    let (_, retry) = destroy(&f, sandbox, "destroy-request-01", json!({})).await;
    assert_eq!(retry["operation_id"], first["operation_id"]);
    assert_eq!(retry["status"], "succeeded");
    let before: String = sqlx::query_scalar("SELECT destroyed_at::text FROM sandboxes")
        .fetch_one(&pool)
        .await
        .unwrap();
    let (_, noop) = destroy(&f, sandbox, "destroy-request-02", json!({})).await;
    assert_eq!(noop["status"], "succeeded");
    assert_ne!(noop["operation_id"], first["operation_id"]);
    let after: String = sqlx::query_scalar("SELECT destroyed_at::text FROM sandboxes")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(before, after);
    let (_, body) = f
        .send("GET", &format!("/v1/sandboxes/{sandbox}"), Value::Null)
        .await;
    assert_eq!(body["observed_state"], "destroyed");
    assert_eq!(body["observation_simulated"], true);
    assert!(body.get("active_operation_id").is_none());
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn concurrent_idempotency_and_changed_payloads_are_resolved_before_state(pool: PgPool) {
    let f = std::sync::Arc::new(Fixture::new(&pool).await);
    let sandbox = running(&f).await;
    let mut tasks = tokio::task::JoinSet::new();
    for _ in 0..8 {
        let f = f.clone();
        tasks.spawn(async move { destroy(&f, sandbox, "same-destroy-key-01", json!({})).await });
    }
    let mut ids = std::collections::BTreeSet::new();
    while let Some(result) = tasks.join_next().await {
        let (status, body) = result.unwrap();
        assert_eq!(status, StatusCode::ACCEPTED, "{body}");
        ids.insert(body["operation_id"].as_str().unwrap().to_owned());
    }
    assert_eq!(ids.len(), 1);
    let (status, _) = destroy(
        &f,
        sandbox,
        "same-destroy-key-01",
        json!({"correlation_id":"changed"}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    let (status, body) = destroy(&f, sandbox, "another-destroy-key", json!({})).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["operation_id"], ids.first().unwrap().as_str());
    assert_eq!(f.controller().await.tick().await.unwrap(), Tick::Confirmed);
    let (status, _) = destroy(&f, sandbox, "another-destroy-key", json!({})).await;
    assert_eq!(status, StatusCode::ACCEPTED);
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn live_create_conflicts_and_rejected_key_is_not_consumed(pool: PgPool) {
    let f = Fixture::new(&pool).await;
    let (create, sandbox) = f.admit().await;
    let (status, body) = destroy(&f, sandbox, "waiting-destroy-key", json!({})).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["operation_id"], create.to_string());
    assert_eq!(f.controller().await.tick().await.unwrap(), Tick::Confirmed);
    assert_eq!(
        destroy(&f, sandbox, "waiting-destroy-key", json!({}))
            .await
            .0,
        StatusCode::ACCEPTED
    );
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn unknown_create_hands_cleanup_to_destroy_and_fences_the_old_claim(pool: PgPool) {
    let f = Fixture::new(&pool).await;
    let (create, sandbox) = f.admit().await;
    let (claim, request) = f.prepared().await;
    let ready = f
        .fake
        .create(tonic::Request::new(request))
        .await
        .unwrap()
        .into_inner();
    f.store.record_create_unknown(&claim).await.unwrap();
    assert_eq!(
        destroy(&f, sandbox, "unknown-cleanup-key", json!({}))
            .await
            .0,
        StatusCode::ACCEPTED
    );
    assert!(
        f.store
            .claim_next(OperationKind::Create, 30)
            .await
            .unwrap()
            .is_none()
    );
    assert!(matches!(
        f.store
            .record_create_observation(&claim, &ready, true)
            .await,
        Err(DispatchError::LostClaim)
    ));
    assert_eq!(f.controller().await.tick().await.unwrap(), Tick::Confirmed);
    assert_eq!(unreleased(&pool).await, 0);
    let (_, body) = f
        .send("GET", &format!("/v1/operations/{create}"), Value::Null)
        .await;
    assert_eq!(body["status"], "failed");
    assert_eq!(body["error"]["create_outcome_unknown"], true);
    assert_eq!(f.fake.total_starts().await, 1);
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn destroy_fences_a_create_that_was_never_sent_and_rejects_a_delayed_start(pool: PgPool) {
    let f = Fixture::new(&pool).await;
    let (_, sandbox) = f.admit().await;
    let (claim, request) = f.prepared().await;
    f.store.record_create_unknown(&claim).await.unwrap();
    assert_eq!(
        destroy(&f, sandbox, "absent-cleanup-key", json!({}))
            .await
            .0,
        StatusCode::ACCEPTED
    );
    assert_eq!(f.controller().await.tick().await.unwrap(), Tick::Confirmed);
    assert_eq!(unreleased(&pool).await, 0);
    assert_eq!(
        f.fake
            .create(tonic::Request::new(request))
            .await
            .unwrap_err()
            .code(),
        tonic::Code::FailedPrecondition
    );
    let proof: Value = sqlx::query_scalar("SELECT release_evidence FROM allocations")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(proof["fenced_absent"], true);
    assert_eq!(f.fake.total_starts().await, 0);
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn lost_stop_reply_retains_capacity_until_release_is_reconciled(pool: PgPool) {
    let f = Fixture::new(&pool).await;
    let sandbox = running(&f).await;
    destroy(&f, sandbox, "lost-stop-reply-key", json!({})).await;
    f.fake.lose_next_stop_reply().await;
    assert_eq!(f.controller().await.tick().await.unwrap(), Tick::Unknown);
    assert_eq!(unreleased(&pool).await, 1);
    f.reclaim_now().await;
    assert_eq!(f.controller().await.tick().await.unwrap(), Tick::Confirmed);
    assert_eq!(unreleased(&pool).await, 0);
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn crash_before_stop_reconciles_then_repeats_only_idempotent_stop(pool: PgPool) {
    let f = Fixture::new(&pool).await;
    let sandbox = running(&f).await;
    destroy(&f, sandbox, "stop-intent-only-key", json!({})).await;
    let old = f
        .store
        .claim_next(OperationKind::Destroy, 30)
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        f.store.prepare_destroy(&old, false).await.unwrap(),
        DestroyAction::Stop(_)
    ));
    f.reclaim_now().await;
    assert_eq!(f.controller().await.tick().await.unwrap(), Tick::Confirmed);
    let attempts: i32 =
        sqlx::query_scalar("SELECT attempt_count FROM operations WHERE kind='destroy'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(attempts, 2);
    assert_eq!(unreleased(&pool).await, 0);
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn plain_absence_and_mismatched_release_are_never_release_proof(pool: PgPool) {
    let f = Fixture::new(&pool).await;
    let (_, sandbox) = f.admit().await;
    let (create, _) = f.prepared().await;
    f.store.record_create_unknown(&create).await.unwrap();
    destroy(&f, sandbox, "unconfirmed-stop-key", json!({})).await;
    let claim = f
        .store
        .claim_next(OperationKind::Destroy, 30)
        .await
        .unwrap()
        .unwrap();
    let DestroyAction::Stop(owner) = f.store.prepare_destroy(&claim, false).await.unwrap() else {
        panic!("stop")
    };
    let absent = f
        .fake
        .inspect(tonic::Request::new(InspectRequest {
            ownership: Some(owner.clone()),
        }))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(absent.state, AllocationState::Absent as i32);
    assert!(matches!(
        f.store
            .record_destroy_observation(&claim, &absent, true)
            .await,
        Err(DispatchError::BadEvidence)
    ));
    assert_eq!(unreleased(&pool).await, 1);
    let stopped = f
        .fake
        .stop(tonic::Request::new(StopRequest {
            ownership: Some(owner),
        }))
        .await
        .unwrap()
        .into_inner();
    for field in ["epoch", "generation", "claim", "allocation", "time"] {
        let mut wrong = stopped.clone();
        let owner = wrong.ownership.as_mut().unwrap();
        match field {
            "epoch" => owner.supervisor_epoch += 1,
            "generation" => owner.generation += 1,
            "claim" => owner.claim_revision += 1,
            "allocation" => owner.allocation_id = "other".into(),
            _ => wrong.observed_unix_ms = 1,
        };
        assert!(matches!(
            f.store
                .record_destroy_observation(&claim, &wrong, true)
                .await,
            Err(DispatchError::BadEvidence)
        ));
    }
    assert!(matches!(
        f.store
            .record_destroy_observation(&claim, &stopped, false)
            .await,
        Err(DispatchError::SimulationDenied)
    ));
    assert_eq!(unreleased(&pool).await, 1);
    f.store
        .record_destroy_observation(&claim, &stopped, true)
        .await
        .unwrap();
    assert_eq!(unreleased(&pool).await, 0);
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn another_project_cannot_destroy_or_discover_a_sandbox(pool: PgPool) {
    let victim = Fixture::new(&pool).await;
    let (_, sandbox) = victim.admit().await;
    let caller = Fixture::new(&pool).await;
    for id in [sandbox, SandboxId::generate()] {
        let (status, body) = destroy(&caller, id, "unauthorized-key-01", json!({})).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body["code"], "not_found");
    }
    let response = caller
        .app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/v1/sandboxes/{sandbox}/destroy"))
                .header("content-type", "application/json")
                .header("idempotency-key", "unauthenticated-key")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM operations WHERE kind='destroy'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn credential_revocation_does_not_block_admitted_safety_cleanup(pool: PgPool) {
    let f = Fixture::new(&pool).await;
    let sandbox = running(&f).await;
    destroy(&f, sandbox, "admitted-cleanup-key", json!({})).await;
    sqlx::query("UPDATE projects SET status='suspended',api_tokens='[]'")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(f.controller().await.tick().await.unwrap(), Tick::Confirmed);
    assert_eq!(unreleased(&pool).await, 0);
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn expired_destroy_claim_cannot_free_capacity_after_waiting_for_locks(pool: PgPool) {
    let f = Fixture::new(&pool).await;
    let sandbox = running(&f).await;
    destroy(&f, sandbox, "expiring-cleanup-key", json!({})).await;
    let claim = f
        .store
        .claim_next(OperationKind::Destroy, 1)
        .await
        .unwrap()
        .unwrap();
    let DestroyAction::Stop(owner) = f.store.prepare_destroy(&claim, false).await.unwrap() else {
        panic!("stop")
    };
    let stopped = f
        .fake
        .stop(tonic::Request::new(StopRequest {
            ownership: Some(owner),
        }))
        .await
        .unwrap()
        .into_inner();
    let mut lock = pool.begin().await.unwrap();
    sqlx::query("SELECT id FROM projects FOR UPDATE")
        .execute(&mut *lock)
        .await
        .unwrap();
    let store = f.store.clone();
    let task = tokio::spawn(async move {
        store
            .record_destroy_observation(&claim, &stopped, true)
            .await
    });
    tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
    lock.commit().await.unwrap();
    assert!(matches!(task.await.unwrap(), Err(DispatchError::LostClaim)));
    assert_eq!(unreleased(&pool).await, 1);
    f.reclaim_now().await;
    assert_eq!(f.controller().await.tick().await.unwrap(), Tick::Confirmed);
    assert_eq!(unreleased(&pool).await, 0);
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn unknown_never_allocated_create_can_be_retired_without_host_evidence(pool: PgPool) {
    let f = Fixture::new(&pool).await;
    let (create, sandbox) = f.admit().await;
    sqlx::query("UPDATE operations SET status='unknown' WHERE id=$1")
        .bind(create.uuid())
        .execute(&pool)
        .await
        .unwrap();
    let (status, body) = destroy(&f, sandbox, "never-allocated-key", json!({})).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(body["status"], "succeeded");
    let old_status: String = sqlx::query_scalar("SELECT status FROM operations WHERE id=$1")
        .bind(create.uuid())
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(old_status, "failed");
    assert_eq!(f.fake.total_starts().await, 0);
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn lost_fenced_absence_reply_is_recovered_without_starting_anything(pool: PgPool) {
    let f = Fixture::new(&pool).await;
    let (_, sandbox) = f.admit().await;
    let (claim, request) = f.prepared().await;
    f.store.record_create_unknown(&claim).await.unwrap();
    destroy(&f, sandbox, "lost-empty-fence-key", json!({})).await;
    f.fake.lose_next_stop_reply().await;
    assert_eq!(f.controller().await.tick().await.unwrap(), Tick::Unknown);
    assert_eq!(unreleased(&pool).await, 1);
    f.reclaim_now().await;
    assert_eq!(f.controller().await.tick().await.unwrap(), Tick::Confirmed);
    assert_eq!(unreleased(&pool).await, 0);
    assert!(f.fake.create(tonic::Request::new(request)).await.is_err());
    assert_eq!(f.fake.total_starts().await, 0);
}
