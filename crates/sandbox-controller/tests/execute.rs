//! Public command admission through PostgreSQL and real mTLS RPCs to the fake.
#![allow(clippy::unwrap_used, clippy::expect_used)]
#[allow(dead_code)]
mod common;
use common::Fixture;
use http::StatusCode;
use sandbox_controller::Tick;
use sandbox_protocol::{
    Id, OperationId, ProjectId, SandboxId,
    supervisor::{CommandRequest, supervisor_server::Supervisor},
};
use sandbox_store::{claims::OperationKind, dispatch::DispatchError, execute::ExecuteAction};
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt;
fn body() -> Value {
    json!({"argv":["/bin/busybox","echo","private-command"],"deadline_unix_ms":sandbox_fake_host::unix_ms().unwrap()+60_000})
}
async fn request(f: &Fixture, sandbox: SandboxId, key: &str, value: Value) -> (StatusCode, Value) {
    let r = http::Request::builder()
        .method("POST")
        .uri(format!("/v1/sandboxes/{sandbox}/execute"))
        .header("authorization", format!("Bearer {}", f.token))
        .header("content-type", "application/json")
        .header("idempotency-key", key)
        .body(axum::body::Body::from(value.to_string()))
        .unwrap();
    let response = f.app.clone().oneshot(r).await.unwrap();
    assert_eq!(response.headers()["cache-control"], "no-store");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 65536)
        .await
        .unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}
async fn ready(f: &Fixture) -> (sandbox_controller::Controller, SandboxId) {
    let (_, s) = f.admit().await;
    let mut c = f.controller().await;
    assert_eq!(c.tick().await.unwrap(), Tick::Confirmed);
    (c, s)
}
async fn state(f: &Fixture, id: &str) -> Value {
    let (status, value) = f
        .send("GET", &format!("/v1/operations/{id}"), Value::Null)
        .await;
    assert_eq!(status, StatusCode::OK);
    value
}
async fn admit(f: &Fixture, s: SandboxId) -> OperationId {
    let (status, value) = request(f, s, &OperationId::generate().to_string(), body()).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{value}");
    value["operation_id"].as_str().unwrap().parse().unwrap()
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn public_execute_succeeds_and_retries_never_dispatch_twice(pool: PgPool) {
    let f = Fixture::new(&pool).await;
    let (mut c, s) = ready(&f).await;
    let key = OperationId::generate().to_string();
    let command = body();
    let (status, admission) = request(&f, s, &key, command.clone()).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let (_, retry) = request(&f, s, &key, command.clone()).await;
    assert_eq!(retry, admission);
    assert_eq!(c.tick().await.unwrap(), Tick::Confirmed);
    let result = state(&f, admission["operation_id"].as_str().unwrap()).await;
    assert_eq!(result["status"], "succeeded");
    assert_eq!(result["output_status"], "pending");
    assert_eq!(result["result"]["exit_code"], 0);
    assert_eq!(result["result"]["simulated"], true);
    assert!(!result.to_string().contains("private-command"));
    let (_, retry) = request(&f, s, &key, command.clone()).await;
    assert_eq!(retry["operation_id"], admission["operation_id"]);
    let mut changed = command;
    changed["argv"] = json!(["changed"]);
    assert_eq!(request(&f, s, &key, changed).await.0, StatusCode::CONFLICT);
    assert_eq!(f.fake.total_commands().await, 1);
    let (n,): (i64,) = sqlx::query_as("SELECT count(*) FROM allocations WHERE released_at IS NULL")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(n, 1);
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn public_capacity_rejection_never_dispatches_and_destroy_still_completes(pool: PgPool) {
    let f = Fixture::new(&pool).await;
    let (mut c, s) = ready(&f).await;
    let mut last = None;
    for _ in 0..sandbox_protocol::command::MAX_COMMANDS {
        let key = OperationId::generate().to_string();
        let command = body();
        let (status, admission) = request(&f, s, &key, command.clone()).await;
        assert_eq!(status, StatusCode::ACCEPTED, "{admission}");
        assert_eq!(c.tick().await.unwrap(), Tick::Confirmed);
        assert_eq!(
            state(&f, admission["operation_id"].as_str().unwrap()).await["status"],
            "succeeded"
        );
        last = Some((key, command, admission));
    }
    let (key, mut command, admission) = last.unwrap();
    let retry = request(&f, s, &key, command.clone()).await;
    assert_eq!(retry.0, StatusCode::ACCEPTED);
    assert_eq!(retry.1["operation_id"], admission["operation_id"]);
    command["argv"] = json!(["changed"]);
    assert_eq!(request(&f, s, &key, command).await.1["code"], "conflict");
    let rejected = request(&f, s, &OperationId::generate().to_string(), body()).await;
    assert_eq!(rejected.0, StatusCode::CONFLICT);
    assert_eq!(rejected.1["code"], "execution_capacity_exhausted");
    assert!(rejected.1.get("operation_id").is_none());
    assert_eq!(c.tick().await.unwrap(), Tick::Idle);
    assert_eq!(
        f.fake.total_commands().await,
        sandbox_protocol::command::MAX_COMMANDS as u64
    );
    let (status, destroy) = f
        .send("POST", &format!("/v1/sandboxes/{s}/destroy"), json!({}))
        .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(c.tick().await.unwrap(), Tick::Confirmed);
    assert_eq!(
        state(&f, destroy["operation_id"].as_str().unwrap()).await["status"],
        "succeeded"
    );
    assert_eq!(
        f.fake.total_commands().await,
        sandbox_protocol::command::MAX_COMMANDS as u64
    );
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn public_output_capacity_rejects_without_consuming_the_retry_key(pool: PgPool) {
    let f = Fixture::new(&pool).await;
    let (mut c, s) = ready(&f).await;
    for _ in 0..6 {
        let mut command = body();
        command["output_limit"] = json!(10 * 1024 * 1024);
        assert_eq!(
            request(&f, s, &OperationId::generate().to_string(), command)
                .await
                .0,
            StatusCode::ACCEPTED
        );
        assert_eq!(c.tick().await.unwrap(), Tick::Confirmed);
    }
    let key = OperationId::generate().to_string();
    let mut command = body();
    command["output_limit"] = json!(4 * 1024 * 1024 + 1);
    let rejected = request(&f, s, &key, command.clone()).await;
    assert_eq!(rejected.0, StatusCode::CONFLICT);
    assert_eq!(rejected.1["code"], "execution_capacity_exhausted");
    command["output_limit"] = json!(4 * 1024 * 1024);
    assert_eq!(request(&f, s, &key, command).await.0, StatusCode::ACCEPTED);
    assert_eq!(c.tick().await.unwrap(), Tick::Confirmed);
    assert_eq!(f.fake.total_commands().await, 7);
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn lost_command_reply_reconciles_one_simulated_start(pool: PgPool) {
    let f = Fixture::new(&pool).await;
    let (mut c, s) = ready(&f).await;
    let id = admit(&f, s).await;
    f.fake.lose_next_command_reply().await;
    assert_eq!(c.tick().await.unwrap(), Tick::Unknown);
    assert_eq!(state(&f, &id.to_string()).await["status"], "unknown");
    assert_eq!(f.fake.total_commands().await, 1);
    f.reclaim_now().await;
    let mut replacement = f.controller().await;
    assert_eq!(replacement.tick().await.unwrap(), Tick::Confirmed);
    assert_eq!(state(&f, &id.to_string()).await["status"], "succeeded");
    assert_eq!(f.fake.total_commands().await, 1);
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn command_outlives_request_reports_nonzero_and_destroy_remains_available(pool: PgPool) {
    let f = Fixture::new(&pool).await;
    let (mut c, s) = ready(&f).await;
    f.fake.hold_next_command().await;
    let id = admit(&f, s).await;
    assert_eq!(c.tick().await.unwrap(), Tick::Confirmed);
    assert_eq!(state(&f, &id.to_string()).await["status"], "running");
    assert_eq!(
        request(&f, s, &OperationId::generate().to_string(), body())
            .await
            .0,
        StatusCode::CONFLICT
    );
    f.fake.finish_command(id, 7).await.unwrap();
    f.reclaim_now().await;
    assert_eq!(c.tick().await.unwrap(), Tick::Confirmed);
    let result = state(&f, &id.to_string()).await;
    assert_eq!(result["status"], "failed");
    assert_eq!(result["result"]["exit_code"], 7);
    f.fake.hold_next_command().await;
    let next = admit(&f, s).await;
    c.tick().await.unwrap();
    let (status, destroy) = f
        .send("POST", &format!("/v1/sandboxes/{s}/destroy"), json!({}))
        .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    for _ in 0..4 {
        f.reclaim_now().await;
        c.tick().await.unwrap();
    }
    assert_eq!(
        state(&f, destroy["operation_id"].as_str().unwrap()).await["status"],
        "succeeded"
    );
    assert_eq!(state(&f, &next.to_string()).await["status"], "unknown");
    assert_eq!(
        request(&f, s, &OperationId::generate().to_string(), body())
            .await
            .0,
        StatusCode::GONE
    );
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn crash_after_intent_before_rpc_installs_a_no_start_fence(pool: PgPool) {
    let f = Fixture::new(&pool).await;
    let (mut c, s) = ready(&f).await;
    let id = admit(&f, s).await;
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
    f.reclaim_now().await;
    assert_eq!(c.tick().await.unwrap(), Tick::Confirmed);
    let result = state(&f, &id.to_string()).await;
    assert_eq!(result["status"], "failed");
    assert_eq!(result["error"]["code"], "command_not_started");
    // A stale sender cannot defeat the inspection's higher revision.
    let late = f
        .fake
        .execute_command(tonic::Request::new(CommandRequest {
            ownership: Some(owner.clone()),
            command: Some((&command).into()),
        }))
        .await;
    assert!(late.is_err());
    let mut newer = owner;
    newer.claim_revision += 10;
    let late = f
        .fake
        .execute_command(tonic::Request::new(CommandRequest {
            ownership: Some(newer),
            command: Some((&command).into()),
        }))
        .await
        .unwrap()
        .into_inner();
    assert!(late.not_started);
    assert_eq!(f.fake.total_commands().await, 0);
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn revoked_credentials_prevent_first_dispatch_but_not_result_reconciliation(pool: PgPool) {
    let f = Fixture::new(&pool).await;
    let (mut c, s) = ready(&f).await;
    let id = admit(&f, s).await;
    sqlx::query("UPDATE projects SET api_tokens=jsonb_set(api_tokens,'{0,revoked_at}',to_jsonb(clock_timestamp()::text))").execute(&pool).await.unwrap();
    assert_eq!(c.tick().await.unwrap(), Tick::Rejected);
    assert_eq!(f.fake.total_commands().await, 0);
    let (status,): (String,) = sqlx::query_as("SELECT status FROM operations WHERE id=$1")
        .bind(id.uuid())
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(status, "failed");
    sqlx::query("UPDATE projects SET api_tokens=api_tokens#-'{0,revoked_at}'")
        .execute(&pool)
        .await
        .unwrap();
    f.fake.lose_next_command_reply().await;
    let id = admit(&f, s).await;
    assert_eq!(c.tick().await.unwrap(), Tick::Unknown);
    sqlx::query("UPDATE projects SET api_tokens=jsonb_set(api_tokens,'{0,revoked_at}',to_jsonb(clock_timestamp()::text))").execute(&pool).await.unwrap();
    f.reclaim_now().await;
    assert_eq!(c.tick().await.unwrap(), Tick::Confirmed);
    let (status,): (String,) = sqlx::query_as("SELECT status FROM operations WHERE id=$1")
        .bind(id.uuid())
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(status, "succeeded");
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn public_execute_validates_scope_shape_and_deadline(pool: PgPool) {
    let f = Fixture::new(&pool).await;
    let (_c, s) = ready(&f).await;
    let other = Fixture::new(&pool).await;
    let (_, other_s) = other.admit().await;
    assert_eq!(
        request(&f, other_s, &OperationId::generate().to_string(), body())
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    for changes in [
        json!({"argv":[]}),
        json!({"output_limit":10485761}),
        json!({"deadline_unix_ms":1}),
        json!({"extra":"unknown"}),
    ] {
        let mut b = body();
        b.as_object_mut()
            .unwrap()
            .extend(changes.as_object().unwrap().clone());
        assert_eq!(
            request(&f, s, &OperationId::generate().to_string(), b)
                .await
                .0,
            StatusCode::BAD_REQUEST
        );
    }
    assert_eq!(
        request(&f, s, "short", body()).await.0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(f.fake.total_commands().await, 0);
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn forged_stale_or_cross_boot_command_evidence_cannot_finish_work(pool: PgPool) {
    let f = Fixture::new(&pool).await;
    let (_c, s) = ready(&f).await;
    f.fake.hold_next_command().await;
    let id = admit(&f, s).await;
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
    let observation = f
        .fake
        .execute_command(tonic::Request::new(CommandRequest {
            ownership: Some(owner),
            command: Some((&command).into()),
        }))
        .await
        .unwrap()
        .into_inner();
    assert!(matches!(
        f.store
            .record_execute_observation(&claim, &observation, false)
            .await,
        Err(DispatchError::SimulationDenied)
    ));
    for which in 0..6 {
        let mut bad = observation.clone();
        match which {
            0 => bad.ownership.as_mut().unwrap().project_id = ProjectId::generate().to_string(),
            1 => bad.command_digest[0] ^= 1,
            2 => bad.observed_unix_ms = 1,
            3 => bad.receipt.as_mut().unwrap().output_limit += 1,
            4 => {
                bad.receipt
                    .as_mut()
                    .unwrap()
                    .context
                    .as_mut()
                    .unwrap()
                    .generation += 1
            }
            _ => bad.not_started = true,
        };
        assert!(matches!(
            f.store.record_execute_observation(&claim, &bad, true).await,
            Err(DispatchError::BadEvidence)
        ));
    }
    f.store
        .record_execute_observation(&claim, &observation, true)
        .await
        .unwrap();
    f.reclaim_now().await;
    let next = f
        .store
        .claim_next(OperationKind::Execute, 30)
        .await
        .unwrap()
        .unwrap();
    let ExecuteAction::Inspect { owner, .. } = f
        .store
        .prepare_execute(&next, f.config.host, 1)
        .await
        .unwrap()
    else {
        panic!("inspect")
    };
    let mut changed = observation.clone();
    changed.ownership = Some(owner);
    changed
        .receipt
        .as_mut()
        .unwrap()
        .context
        .as_mut()
        .unwrap()
        .boot_id = "different-boot".into();
    assert!(matches!(
        f.store
            .record_execute_observation(&next, &changed, true)
            .await,
        Err(DispatchError::BadEvidence)
    ));
    assert!(matches!(
        f.store
            .record_execute_observation(&claim, &observation, true)
            .await,
        Err(DispatchError::LostClaim)
    ));
    let (n,): (i64,) = sqlx::query_as(
        "SELECT jsonb_array_length(attempt_receipts)::bigint FROM operations WHERE id=$1",
    )
    .bind(id.uuid())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(n, 2);
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn command_receipt_pressure_preserves_destroy_fence_capacity(pool: PgPool) {
    use sandbox_protocol::supervisor::CommandInspection;
    let f = Fixture::new(&pool).await;
    let (mut c, s) = ready(&f).await;
    let _ = admit(&f, s).await;
    let claim = f
        .store
        .claim_next(OperationKind::Execute, 30)
        .await
        .unwrap()
        .unwrap();
    let ExecuteAction::Dispatch { owner, .. } = f
        .store
        .prepare_execute(&claim, f.config.host, 1)
        .await
        .unwrap()
    else {
        panic!("intent")
    };
    for _ in 0..sandbox_protocol::command::MAX_COMMANDS {
        let mut unique = owner.clone();
        unique.operation_id = OperationId::generate().to_string();
        assert!(
            f.fake
                .inspect_command(tonic::Request::new(CommandInspection {
                    ownership: Some(unique),
                    command_digest: vec![0; 32]
                }))
                .await
                .unwrap()
                .into_inner()
                .not_started
        );
    }
    for _ in 0..70 {
        let mut unique = owner.clone();
        unique.operation_id = OperationId::generate().to_string();
        assert_eq!(
            f.fake
                .inspect_command(tonic::Request::new(CommandInspection {
                    ownership: Some(unique),
                    command_digest: vec![0; 32]
                }))
                .await
                .unwrap_err()
                .code(),
            tonic::Code::ResourceExhausted
        );
    }
    let (status, destroy) = f
        .send("POST", &format!("/v1/sandboxes/{s}/destroy"), json!({}))
        .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(c.tick().await.unwrap(), Tick::Confirmed);
    assert_eq!(
        state(&f, destroy["operation_id"].as_str().unwrap()).await["status"],
        "succeeded"
    );
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn completion_expiring_during_lock_wait_rolls_back_all_result_writes(pool: PgPool) {
    let f = Fixture::new(&pool).await;
    let (_c, s) = ready(&f).await;
    let id = admit(&f, s).await;
    let claim = f
        .store
        .claim_next(OperationKind::Execute, 1)
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
    let observation = f
        .fake
        .execute_command(tonic::Request::new(CommandRequest {
            ownership: Some(owner),
            command: Some((&command).into()),
        }))
        .await
        .unwrap()
        .into_inner();
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("SELECT id FROM hosts WHERE id=$1 FOR UPDATE")
        .bind(f.config.host.uuid())
        .execute(&mut *tx)
        .await
        .unwrap();
    let store = f.store.clone();
    let task = tokio::spawn(async move {
        store
            .record_execute_observation(&claim, &observation, true)
            .await
    });
    tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
    tx.commit().await.unwrap();
    assert!(matches!(task.await.unwrap(), Err(DispatchError::LostClaim)));
    let (status,complete,receipts,result):(String,bool,i32,Option<Value>)=sqlx::query_as("SELECT status,completed_at IS NOT NULL,jsonb_array_length(attempt_receipts),result FROM operations WHERE id=$1")
        .bind(id.uuid()).fetch_one(&pool).await.unwrap();
    assert_eq!(
        (status.as_str(), complete, receipts, result),
        ("running", false, 1, None)
    );
}
