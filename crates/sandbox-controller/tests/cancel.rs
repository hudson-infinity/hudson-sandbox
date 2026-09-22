//! Public cancellation through PostgreSQL, HTTP auth and real mTLS to the fake.
#![allow(clippy::unwrap_used, clippy::expect_used)]
#[allow(dead_code)]
mod common;
use common::Fixture;
use http::StatusCode;
use sandbox_controller::{Controller, Tick};
use sandbox_protocol::{
    Id, OperationId, SandboxId,
    supervisor::{CommandInspection, CommandRequest, supervisor_server::Supervisor},
};
use sandbox_store::{claims::OperationKind, execute::ExecuteAction};
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt;

async fn ready(f: &Fixture) -> (Controller, SandboxId) {
    let (_, s) = f.admit().await;
    let mut c = f.controller().await;
    assert_eq!(c.tick().await.unwrap(), Tick::Confirmed);
    (c, s)
}
async fn execute(f: &Fixture, s: SandboxId) -> OperationId {
    let (status,value)=f.send("POST",&format!("/v1/sandboxes/{s}/execute"),json!({"argv":["private-cancel-test"],"deadline_unix_ms":sandbox_fake_host::unix_ms().unwrap()+60000})).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{value}");
    value["operation_id"].as_str().unwrap().parse().unwrap()
}
async fn cancel(f: &Fixture, id: OperationId, key: &str, body: Value) -> (StatusCode, Value) {
    let r = http::Request::builder()
        .method("POST")
        .uri(format!("/v1/operations/{id}/cancel"))
        .header("authorization", format!("Bearer {}", f.token))
        .header("content-type", "application/json")
        .header("idempotency-key", key)
        .body(axum::body::Body::from(body.to_string()))
        .unwrap();
    let response = f.app.clone().oneshot(r).await.unwrap();
    assert_eq!(response.headers()["cache-control"], "no-store");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 65536)
        .await
        .unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}
async fn state(f: &Fixture, id: OperationId) -> Value {
    let (status, v) = f
        .send("GET", &format!("/v1/operations/{id}"), Value::Null)
        .await;
    assert_eq!(status, StatusCode::OK, "{v}");
    v
}
async fn settled(f: &Fixture, c: &mut Controller, id: OperationId) -> Value {
    for _ in 0..12 {
        f.reclaim_now().await;
        c.tick().await.unwrap();
        let v = state(f, id).await;
        if v["status"] == "succeeded" {
            return v;
        }
    }
    panic!("cancellation did not settle: {}", state(f, id).await)
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn running_cancel_retries_once_preserves_output_and_allows_next_command(pool: PgPool) {
    let f = Fixture::new(&pool).await;
    let (mut c, s) = ready(&f).await;
    f.fake.hold_next_command().await;
    let target = execute(&f, s).await;
    c.tick().await.unwrap();
    assert_eq!(state(&f, target).await["status"], "running");
    let key = OperationId::generate().to_string();
    let (status, admission) = cancel(&f, target, &key, json!({})).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(cancel(&f, target, &key, json!({})).await.1, admission);
    let id: OperationId = admission["operation_id"].as_str().unwrap().parse().unwrap();
    assert_eq!(
        state(&f, id).await["target_operation_id"],
        target.to_string()
    );
    let other = cancel(&f, target, &OperationId::generate().to_string(), json!({})).await;
    assert_eq!(other.0, StatusCode::CONFLICT);
    assert_eq!(other.1["operation_id"], id.to_string());
    let result = settled(&f, &mut c, id).await;
    assert_eq!(result["result"]["cancelled"], true);
    let final_target = state(&f, target).await;
    assert_eq!(final_target["status"], "cancelled");
    assert_eq!(final_target["output_status"], "pending");
    assert_eq!(f.fake.total_commands().await, 1);
    assert_eq!(
        cancel(&f, target, &key, json!({})).await.1["operation_id"],
        id.to_string()
    );
    // An idempotent cancel retry can return before SQLx finishes rolling back
    // its read-only transaction. Output workers intentionally skip locked rows.
    // Acquire the target explicitly (waiting for that rollback), demonstrate
    // the skip, then release it before asserting that publication is available.
    let mut reader = f.store.pool().begin().await.unwrap();
    sqlx::query("SELECT id FROM operations WHERE id=$1 FOR UPDATE")
        .bind(target.uuid())
        .fetch_one(&mut *reader)
        .await
        .unwrap();
    assert!(f.store.claim_output(30).await.unwrap().is_none());
    reader.rollback().await.unwrap();
    let publication = f.store.claim_output(30).await.unwrap().unwrap();
    assert_eq!(publication.operation_id, target);
    let work = f
        .store
        .prepare_output(&publication, 60, 1, true)
        .await
        .unwrap();
    assert_eq!(
        work.receipt.state,
        sandbox_protocol::guest_model::State::Cancelled
    );
    let next = execute(&f, s).await;
    settled(&f, &mut c, next).await;
    assert_eq!(f.fake.total_commands().await, 2);
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn lost_cancel_reply_reconciles_without_reexecution(pool: PgPool) {
    let f = Fixture::new(&pool).await;
    let (mut c, s) = ready(&f).await;
    f.fake.hold_next_command().await;
    let target = execute(&f, s).await;
    c.tick().await.unwrap();
    let (_, a) = cancel(&f, target, &OperationId::generate().to_string(), json!({})).await;
    let id = a["operation_id"].as_str().unwrap().parse().unwrap();
    f.fake.lose_next_cancel_reply().await;
    let mut unknown = false;
    for _ in 0..5 {
        f.reclaim_now().await;
        if c.tick().await.unwrap() == Tick::Unknown {
            unknown = true;
            break;
        }
    }
    assert!(unknown);
    assert_eq!(state(&f, target).await["status"], "unknown");
    let mut replacement = f.controller().await;
    assert_eq!(
        settled(&f, &mut replacement, id).await["result"]["cancelled"],
        true
    );
    assert_eq!(f.fake.total_commands().await, 1);
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn queued_cancel_never_starts_and_destroy_still_completes(pool: PgPool) {
    let f = Fixture::new(&pool).await;
    let (mut c, s) = ready(&f).await;
    let target = execute(&f, s).await;
    let (_, a) = cancel(&f, target, &OperationId::generate().to_string(), json!({})).await;
    let id = a["operation_id"].as_str().unwrap().parse().unwrap();
    assert_eq!(settled(&f, &mut c, id).await["result"]["cancelled"], true);
    assert_eq!(
        state(&f, target).await["result"]["dispatch_intent_absent"],
        true
    );
    assert_eq!(f.fake.total_commands().await, 0);
    let (_, d) = f
        .send("POST", &format!("/v1/sandboxes/{s}/destroy"), json!({}))
        .await;
    let destroy = d["operation_id"].as_str().unwrap().parse().unwrap();
    settled(&f, &mut c, destroy).await;
    assert_eq!(f.fake.total_commands().await, 0);
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn cancel_after_intent_fences_a_delayed_execute(pool: PgPool) {
    let f = Fixture::new(&pool).await;
    let (mut c, s) = ready(&f).await;
    let target = execute(&f, s).await;
    let claim = f
        .store
        .claim_next(OperationKind::Execute, 30)
        .await
        .unwrap()
        .unwrap();
    let ExecuteAction::Dispatch { owner, command } = f
        .store
        .prepare_execute(&claim, f.config.host, 1)
        .await
        .unwrap()
    else {
        panic!("dispatch")
    };
    let (_, a) = cancel(&f, target, &OperationId::generate().to_string(), json!({})).await;
    let id = a["operation_id"].as_str().unwrap().parse().unwrap();
    assert_eq!(settled(&f, &mut c, id).await["result"]["cancelled"], true);
    let mut late = owner;
    late.claim_revision += 100;
    let response = f
        .fake
        .execute_command(tonic::Request::new(CommandRequest {
            ownership: Some(late),
            command: Some((&command).into()),
        }))
        .await
        .unwrap()
        .into_inner();
    assert!(response.not_started);
    assert_eq!(f.fake.total_commands().await, 0);
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn natural_completion_wins_and_missing_guest_is_never_confirmed_cancelled(pool: PgPool) {
    let f = Fixture::new(&pool).await;
    let (mut c, s) = ready(&f).await;
    f.fake.hold_next_command().await;
    let target = execute(&f, s).await;
    c.tick().await.unwrap();
    let (_, a) = cancel(&f, target, &OperationId::generate().to_string(), json!({})).await;
    let id = a["operation_id"].as_str().unwrap().parse().unwrap();
    f.fake.finish_command(target, 7).await.unwrap();
    let result = settled(&f, &mut c, id).await;
    assert_eq!(result["result"]["cancelled"], false);
    assert_eq!(result["result"]["target_status"], "failed");
    assert_eq!(state(&f, target).await["result"]["exit_code"], 7);
    // A separate target loses its guest through destroy before cancellation.
    f.fake.hold_next_command().await;
    let lost = execute(&f, s).await;
    settled_execution_running(&f, &mut c, lost).await;
    let (_, d) = f
        .send("POST", &format!("/v1/sandboxes/{s}/destroy"), json!({}))
        .await;
    settled(
        &f,
        &mut c,
        d["operation_id"].as_str().unwrap().parse().unwrap(),
    )
    .await;
    let (_, a) = cancel(&f, lost, &OperationId::generate().to_string(), json!({})).await;
    let id = a["operation_id"].as_str().unwrap().parse().unwrap();
    for _ in 0..6 {
        f.reclaim_now().await;
        c.tick().await.unwrap();
    }
    assert_eq!(state(&f, lost).await["status"], "unknown");
    assert_ne!(state(&f, id).await["status"], "succeeded");
    assert_eq!(f.fake.total_commands().await, 2);
}
async fn settled_execution_running(f: &Fixture, c: &mut Controller, id: OperationId) {
    for _ in 0..6 {
        f.reclaim_now().await;
        c.tick().await.unwrap();
        if state(f, id).await["status"] == "running" {
            return;
        }
    }
    panic!("execution not running");
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn cancel_auth_scope_validation_and_expired_retry_contract(pool: PgPool) {
    let f = Fixture::new(&pool).await;
    let (mut c, s) = ready(&f).await;
    let target = execute(&f, s).await;
    let other = Fixture::new(&pool).await;
    let foreign = cancel(
        &other,
        target,
        &OperationId::generate().to_string(),
        json!({}),
    )
    .await;
    let missing = cancel(
        &other,
        OperationId::generate(),
        &OperationId::generate().to_string(),
        json!({}),
    )
    .await;
    assert_eq!(foreign, missing);
    assert_eq!(foreign.0, StatusCode::NOT_FOUND);
    let key = OperationId::generate().to_string();
    assert_eq!(
        cancel(&f, target, &key, json!({"allocation_id":"injected"}))
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
    let (_, a) = cancel(&f, target, &key, json!({})).await;
    let id: OperationId = a["operation_id"].as_str().unwrap().parse().unwrap();
    assert_eq!(
        cancel(&f, id, &OperationId::generate().to_string(), json!({}))
            .await
            .0,
        StatusCode::CONFLICT
    );
    settled(&f, &mut c, id).await;
    sqlx::query("UPDATE operations SET response_expires_at=clock_timestamp()-interval '1 second' WHERE id=$1").bind(id.uuid()).execute(&pool).await.unwrap();
    f.store.compact_expired_response().await.unwrap();
    let retry = cancel(&f, target, &key, json!({})).await;
    assert_eq!(retry.0, StatusCode::GONE);
    assert_eq!(retry.1["code"], "response_expired");
    assert_eq!(retry.1["operation_id"], id.to_string());
    assert_eq!(
        cancel(&f, OperationId::generate(), &key, json!({})).await.0,
        StatusCode::CONFLICT
    );
    sqlx::query("UPDATE projects SET api_tokens='[]' WHERE id=(SELECT project_id FROM operations WHERE id=$1)").bind(id.uuid()).execute(&pool).await.unwrap();
    assert_eq!(
        cancel(&f, target, &key, json!({})).await.0,
        StatusCode::UNAUTHORIZED
    );
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn cancel_rpc_rejects_changed_digest_and_stale_ownership(pool: PgPool) {
    let f = Fixture::new(&pool).await;
    let (mut c, s) = ready(&f).await;
    let _ = execute(&f, s).await;
    let claim = f
        .store
        .claim_next(OperationKind::Execute, 30)
        .await
        .unwrap()
        .unwrap();
    let ExecuteAction::Dispatch { owner, command } = f
        .store
        .prepare_execute(&claim, f.config.host, 1)
        .await
        .unwrap()
    else {
        panic!("dispatch")
    };
    f.fake.hold_next_command().await;
    f.fake
        .execute_command(tonic::Request::new(CommandRequest {
            ownership: Some(owner.clone()),
            command: Some((&command).into()),
        }))
        .await
        .unwrap();
    let bad = f
        .fake
        .cancel_command(tonic::Request::new(CommandInspection {
            ownership: Some(owner.clone()),
            command_digest: vec![0; 32],
        }))
        .await
        .unwrap_err();
    assert_eq!(bad.code(), tonic::Code::AlreadyExists);
    let mut newer = owner.clone();
    newer.claim_revision += 1;
    f.fake
        .inspect_command(tonic::Request::new(CommandInspection {
            ownership: Some(newer),
            command_digest: command.digest().unwrap().to_vec(),
        }))
        .await
        .unwrap();
    let stale = f
        .fake
        .cancel_command(tonic::Request::new(CommandInspection {
            ownership: Some(owner),
            command_digest: command.digest().unwrap().to_vec(),
        }))
        .await
        .unwrap_err();
    assert_eq!(stale.code(), tonic::Code::FailedPrecondition);
    let (_, d) = f
        .send("POST", &format!("/v1/sandboxes/{s}/destroy"), json!({}))
        .await;
    settled(
        &f,
        &mut c,
        d["operation_id"].as_str().unwrap().parse().unwrap(),
    )
    .await;
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn queued_cancel_completes_when_host_health_fails_and_revokes_old_claim(pool: PgPool) {
    let f = Fixture::new(&pool).await;
    let (mut c, s) = ready(&f).await;
    let target = execute(&f, s).await;
    let old = f
        .store
        .claim_next(OperationKind::Execute, 30)
        .await
        .unwrap()
        .unwrap();
    let (_, a) = cancel(&f, target, &OperationId::generate().to_string(), json!({})).await;
    let id = a["operation_id"].as_str().unwrap().parse().unwrap();
    f.fake.fail_next_health_check().await;
    assert_eq!(c.tick().await.unwrap(), Tick::Confirmed);
    assert_eq!(state(&f, id).await["result"]["cancelled"], true);
    assert!(matches!(
        f.store.prepare_execute(&old, f.config.host, 1).await,
        Err(sandbox_store::dispatch::DispatchError::LostClaim)
    ));
    assert_eq!(f.fake.total_commands().await, 0);
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn stalled_cancel_database_work_is_bounded_and_does_not_block_destroy(pool: PgPool) {
    let f = Fixture::new(&pool).await;
    let (mut c, s) = ready(&f).await;
    f.fake.hold_next_command().await;
    let target = execute(&f, s).await;
    c.tick().await.unwrap();
    let (_, a) = cancel(&f, target, &OperationId::generate().to_string(), json!({})).await;
    let id = a["operation_id"].as_str().unwrap().parse().unwrap();
    let mut lock = pool.begin().await.unwrap();
    sqlx::query("SELECT id FROM operations WHERE id=$1 FOR UPDATE")
        .bind(target.uuid())
        .execute(&mut *lock)
        .await
        .unwrap();
    let (_, d) = f
        .send("POST", &format!("/v1/sandboxes/{s}/destroy"), json!({}))
        .await;
    let destroy = d["operation_id"].as_str().unwrap().parse().unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), c.tick())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(state(&f, destroy).await["status"], "succeeded");
    assert_ne!(state(&f, id).await["status"], "succeeded");
    lock.commit().await.unwrap();
    assert_eq!(f.fake.total_commands().await, 1);
}
