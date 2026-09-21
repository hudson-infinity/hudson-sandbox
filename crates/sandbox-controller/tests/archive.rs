//! Actual PostgreSQL, authenticated supervisor RPC, and private MinIO objects.
#![allow(clippy::unwrap_used, clippy::expect_used)]
#[allow(dead_code)]
mod common;
use common::Fixture;
use http::StatusCode;
use sandbox_artifacts::{ArtifactStore, S3Config};
use sandbox_controller::{
    Controller, Tick,
    archive::{ArchiveError, ArchiveTick},
};
use sandbox_protocol::{
    Id, OperationId, ProjectId, SandboxId,
    output::OutputPlans,
    supervisor::{OutputRequest, supervisor_server::Supervisor},
};
use serde_json::{Value, json};
use sqlx::PgPool;
use std::time::Duration;
fn artifacts() -> ArtifactStore {
    S3Config {
        endpoint: std::env::var("HUDSON_TEST_S3_ENDPOINT").unwrap(),
        region: "us-east-1".into(),
        bucket: std::env::var("HUDSON_TEST_S3_BUCKET").unwrap(),
        access_key: std::env::var("HUDSON_TEST_S3_ACCESS_KEY").unwrap(),
        secret_key: std::env::var("HUDSON_TEST_S3_SECRET_KEY").unwrap(),
        session_token: None,
        allow_loopback_http: true,
    }
    .build()
    .unwrap()
}
async fn executed(f: &Fixture) -> (Controller, SandboxId, OperationId) {
    let (_, sandbox) = f.admit().await;
    let mut controller = f.controller().await;
    assert_eq!(controller.tick().await.unwrap(), Tick::Confirmed);
    let (status, body) = f.send("POST", &format!("/v1/sandboxes/{sandbox}/execute"), json!({"argv":["/bin/echo","never actually run by fake"],"deadline_unix_ms":sandbox_fake_host::unix_ms().unwrap()+60_000})).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert_eq!(controller.tick().await.unwrap(), Tick::Confirmed);
    (
        controller,
        sandbox,
        body["operation_id"].as_str().unwrap().parse().unwrap(),
    )
}
async fn snapshot(pool: &PgPool, id: OperationId) -> Value {
    sqlx::query_scalar("SELECT jsonb_build_object('status',status,'result',result,'completed_at',completed_at,'attempt_count',attempt_count,'receipts',attempt_receipts,'allocation',execution_allocation_id) FROM operations WHERE id=$1").bind(id.uuid()).fetch_one(pool).await.unwrap()
}
async fn destroy(f: &Fixture, c: &mut Controller, s: SandboxId) {
    let (status, body) = f
        .send("POST", &format!("/v1/sandboxes/{s}/destroy"), json!({}))
        .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(c.tick().await.unwrap(), Tick::Confirmed);
    let (_, result) = f
        .send(
            "GET",
            &format!("/v1/operations/{}", body["operation_id"].as_str().unwrap()),
            Value::Null,
        )
        .await;
    assert_eq!(result["status"], "succeeded");
}
async fn published(f: &Fixture, artifacts: &ArtifactStore, id: OperationId) {
    let (project,): (uuid::Uuid,) = sqlx::query_as("SELECT project_id FROM operations WHERE id=$1")
        .bind(id.uuid())
        .fetch_one(f.store.pool())
        .await
        .unwrap();
    let project = ProjectId::from_uuid(project);
    let view = f
        .store
        .output_for_project(project, id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(view.status, "published");
    let owner = view.owner.unwrap();
    let refs = view.references.unwrap();
    for reference in [&refs.stdout, &refs.stderr] {
        let chunk = artifacts
            .read(
                reference,
                &owner,
                sandbox_fake_host::unix_ms().unwrap(),
                0,
                1024,
            )
            .await
            .unwrap();
        assert!(chunk.eof && chunk.bytes.is_empty() && !chunk.truncated);
    }
    assert!(
        f.store
            .output_for_project(ProjectId::generate(), id)
            .await
            .unwrap()
            .is_none()
    );
    let (_, public) = f
        .send("GET", &format!("/v1/operations/{id}"), Value::Null)
        .await;
    assert_eq!(public["output_status"], "published");
    assert!(!public.to_string().contains("object_key"));
    assert!(!public.to_string().contains("sha256"));
    assert_eq!(f.fake.total_commands().await, 1);
}
#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
#[ignore = "requires HUDSON_TEST_S3_* private MinIO and PostgreSQL"]
async fn output_minio_lost_ack_recovers_after_destroy_without_reexecuting(pool: PgPool) {
    let artifacts = artifacts();
    let f = Fixture::with_artifacts(&pool, Some(artifacts.clone())).await;
    let (mut controller, sandbox, id) = executed(&f).await;
    let before = snapshot(&pool, id).await;
    f.fake.lose_next_archive_reply().await;
    assert!(matches!(
        controller.output_archiver(3600, 60).unwrap().tick().await,
        Err(ArchiveError::Rpc)
    ));
    let (status, plans): (String, Value) =
        sqlx::query_as("SELECT output_status,output_plan FROM operations WHERE id=$1")
            .bind(id.uuid())
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(status, "uploading");
    let plans: OutputPlans = serde_json::from_value(plans).unwrap();
    // Prove that the lost RPC reply followed durable object writes.
    for p in [&plans.stdout, &plans.stderr] {
        artifacts
            .reconcile(p, &p.owner, sandbox_fake_host::unix_ms().unwrap())
            .await
            .unwrap();
    }
    destroy(&f, &mut controller, sandbox).await;
    sqlx::query("UPDATE operations SET output_next_retry_at=NULL WHERE id=$1")
        .bind(id.uuid())
        .execute(&pool)
        .await
        .unwrap();
    let replacement = f.controller().await;
    assert_eq!(
        replacement
            .output_archiver(3600, 60)
            .unwrap()
            .tick()
            .await
            .unwrap(),
        ArchiveTick::Published
    );
    assert_eq!(snapshot(&pool, id).await, before);
    published(&f, &artifacts, id).await;
    assert_eq!(
        replacement
            .output_archiver(3600, 60)
            .unwrap()
            .tick()
            .await
            .unwrap(),
        ArchiveTick::Idle
    );
}
#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
#[ignore = "requires HUDSON_TEST_S3_* private MinIO and PostgreSQL"]
async fn output_minio_slow_archive_does_not_block_renewal_or_destroy(pool: PgPool) {
    let artifacts = artifacts();
    let f = Fixture::with_artifacts(&pool, Some(artifacts.clone())).await;
    let (mut controller, sandbox, id) = executed(&f).await;
    f.fake.lose_next_archive_reply().await;
    assert!(
        controller
            .output_archiver(3600, 60)
            .unwrap()
            .tick()
            .await
            .is_err()
    );
    sqlx::query("UPDATE operations SET output_next_retry_at=NULL WHERE id=$1")
        .bind(id.uuid())
        .execute(&pool)
        .await
        .unwrap();
    let started = f.fake.archives_started().await;
    f.fake.delay_archives(Duration::from_secs(6)).await;
    let mut archiver = controller.output_archiver(3600, 60).unwrap();
    let task = tokio::spawn(async move { archiver.tick().await });
    tokio::time::timeout(Duration::from_secs(2), async {
        while f.fake.archives_started().await == started {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    sqlx::query("UPDATE allocations SET maintenance_next_at=clock_timestamp()-interval '1 second' WHERE released_at IS NULL").execute(&pool).await.unwrap();
    tokio::time::timeout(Duration::from_secs(1), controller.tick())
        .await
        .unwrap()
        .unwrap();
    let (revision,): (i64,) =
        sqlx::query_as("SELECT maintenance_revision FROM allocations WHERE released_at IS NULL")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(revision > 0);
    tokio::time::timeout(
        Duration::from_secs(1),
        destroy(&f, &mut controller, sandbox),
    )
    .await
    .unwrap();
    assert!(
        !task.is_finished(),
        "archival must still be delayed during lifecycle work"
    );
    assert_eq!(task.await.unwrap().unwrap(), ArchiveTick::Published);
    published(&f, &artifacts, id).await;
}
#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
#[ignore = "requires HUDSON_TEST_S3_* private MinIO and PostgreSQL"]
async fn output_minio_missing_guest_is_not_empty_and_stale_plans_are_rejected(pool: PgPool) {
    let artifacts = artifacts();
    let f = Fixture::with_artifacts(&pool, Some(artifacts)).await;
    let (mut controller, sandbox, id) = executed(&f).await;
    let claim = f.store.claim_output(120).await.unwrap().unwrap();
    let work = f
        .store
        .prepare_output(&claim, 3600, 60, true)
        .await
        .unwrap();
    let request = OutputRequest {
        ticket_json: serde_json::to_vec(&work.ticket).unwrap(),
        publication_revision: claim.revision,
        claim_expires_unix_ms: sandbox_fake_host::unix_ms().unwrap() + 60_000,
        plans_json: vec![],
    };
    let response = f
        .fake
        .prepare_output(tonic::Request::new(request.clone()))
        .await
        .unwrap()
        .into_inner();
    let plans: OutputPlans = serde_json::from_slice(&response.plans_json).unwrap();
    f.store
        .save_output_plans(&claim, &plans, true)
        .await
        .unwrap();
    let mut newer = request.clone();
    newer.publication_revision += 1;
    f.fake
        .prepare_output(tonic::Request::new(newer.clone()))
        .await
        .unwrap();
    let mut stale = request;
    stale.plans_json = response.plans_json.clone();
    assert_eq!(
        f.fake
            .archive_output(tonic::Request::new(stale))
            .await
            .unwrap_err()
            .code(),
        tonic::Code::FailedPrecondition
    );
    let mut changed = plans.clone();
    changed.stdout.sha256 = "a".repeat(64);
    newer.plans_json = serde_json::to_vec(&changed).unwrap();
    assert!(
        f.fake
            .archive_output(tonic::Request::new(newer.clone()))
            .await
            .is_err()
    );
    destroy(&f, &mut controller, sandbox).await;
    newer.plans_json = response.plans_json;
    assert_eq!(
        f.fake
            .archive_output(tonic::Request::new(newer))
            .await
            .unwrap_err()
            .code(),
        tonic::Code::Unavailable
    );
    let (status, refs): (String, Value) =
        sqlx::query_as("SELECT output_status,output_refs FROM operations WHERE id=$1")
            .bind(id.uuid())
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(status, "uploading");
    assert_eq!(refs, json!([]));
    assert_eq!(f.fake.total_commands().await, 1);
}
