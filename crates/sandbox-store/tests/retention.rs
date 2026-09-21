//! Retention scheduling uses real PostgreSQL and synthetic operation history.
#![allow(clippy::unwrap_used, clippy::expect_used)]
use sandbox_protocol::{Id, OperationId, ProjectId, SandboxId};
use sandbox_store::{Store, retention::ResponseRetention};
use serde_json::Value;
use sqlx::PgPool;

async fn seed(pool: &PgPool, count: usize) -> Vec<OperationId> {
    let project = ProjectId::generate();
    let sandbox = SandboxId::generate();
    sqlx::query("INSERT INTO projects(id,name,status,limits) VALUES($1,'retention','active','{}')")
        .bind(project.uuid())
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO sandboxes(id,project_id,image_digest,resources,desired_state,observed_state) VALUES($1,$2,'sha256:fixture','{}','destroyed','destroyed')")
        .bind(sandbox.uuid()).bind(project.uuid()).execute(pool).await.unwrap();
    let mut ids = Vec::new();
    for _ in 0..count {
        let id = OperationId::generate();
        sqlx::query("INSERT INTO operations(id,project_id,sandbox_id,kind,initiator_kind,idempotency_key,request_digest,digest_version,payload,status,completed_at,result) VALUES($1,$2,$3,'create','service',$4,$5,1,'{}','failed',clock_timestamp()-interval '2 days','{}')")
            .bind(id.uuid()).bind(project.uuid()).bind(sandbox.uuid()).bind(id.to_string()).bind([7u8;32].as_slice()).execute(pool).await.unwrap();
        ids.push(id);
    }
    ids
}
async fn snapshot(pool: &PgPool) -> Vec<Value> {
    sqlx::query_scalar("SELECT to_jsonb(o)-'response_expires_at' FROM operations o ORDER BY id")
        .fetch_all(pool)
        .await
        .unwrap()
}
fn policy(seconds: u32) -> ResponseRetention {
    ResponseRetention::new(seconds).unwrap()
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn retention_is_bounded_measured_from_completion_and_never_rewrites_evidence(pool: PgPool) {
    let ids = seed(&pool, 105).await;
    for (index, status) in ["queued", "running", "unknown"].into_iter().enumerate() {
        sqlx::query("UPDATE operations SET status=$2,completed_at=NULL WHERE id=$1")
            .bind(ids[index].uuid())
            .bind(status)
            .execute(&pool)
            .await
            .unwrap();
    }
    sqlx::query(
        "UPDATE operations SET response_expires_at=completed_at+interval '30 days' WHERE id=$1",
    )
    .bind(ids[3].uuid())
    .execute(&pool)
    .await
    .unwrap();
    let before = snapshot(&pool).await;
    let store = Store::from_pool(pool.clone());
    assert_eq!(
        store.assign_response_retention(policy(3600)).await.unwrap(),
        100
    );
    assert_eq!(
        store.assign_response_retention(policy(3600)).await.unwrap(),
        1
    );
    assert_eq!(
        store
            .assign_response_retention(policy(31_536_000))
            .await
            .unwrap(),
        0
    );
    assert_eq!(before, snapshot(&pool).await);
    let assigned:i64=sqlx::query_scalar("SELECT count(*) FROM operations WHERE response_expires_at=completed_at+interval '1 hour' AND response_expires_at<clock_timestamp()")
        .fetch_one(&pool).await.unwrap();
    assert_eq!(assigned, 101);
    let unresolved: i64 =
        sqlx::query_scalar("SELECT count(*) FROM operations WHERE response_expires_at IS NULL")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(unresolved, 3);
    let unchanged: bool = sqlx::query_scalar(
        "SELECT response_expires_at=completed_at+interval '30 days' FROM operations WHERE id=$1",
    )
    .bind(ids[3].uuid())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(unchanged);
    assert!(ResponseRetention::new(0).is_none());
    assert!(ResponseRetention::new(31_536_001).is_none());
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn concurrent_retention_workers_skip_locks_and_assign_each_deadline_once(pool: PgPool) {
    let ids = seed(&pool, 202).await;
    let mut lock = pool.begin().await.unwrap();
    sqlx::query("SELECT id FROM operations WHERE id=$1 FOR UPDATE")
        .bind(ids[0].uuid())
        .fetch_one(&mut *lock)
        .await
        .unwrap();
    let store = Store::from_pool(pool.clone());
    let (a, b, c) = tokio::join!(
        store.assign_response_retention(policy(60)),
        store.assign_response_retention(policy(60)),
        store.assign_response_retention(policy(60))
    );
    assert_eq!(a.unwrap() + b.unwrap() + c.unwrap(), 201);
    let locked: bool =
        sqlx::query_scalar("SELECT response_expires_at IS NULL FROM operations WHERE id=$1")
            .bind(ids[0].uuid())
            .fetch_one(&mut *lock)
            .await
            .unwrap();
    assert!(locked);
    lock.commit().await.unwrap();
    assert_eq!(
        store.assign_response_retention(policy(60)).await.unwrap(),
        1
    );
    assert_eq!(
        store.assign_response_retention(policy(600)).await.unwrap(),
        0
    );
}

#[sqlx::test(migrations = false)]
async fn retention_upgrade_preserves_existing_rows_without_assigning_policy(pool: PgPool) {
    let old = sqlx::migrate::Migrator {
        migrations: std::borrow::Cow::Owned(
            sandbox_store::MIGRATOR.iter().take(8).cloned().collect(),
        ),
        ..sqlx::migrate::Migrator::DEFAULT
    };
    old.run(&pool).await.unwrap();
    seed(&pool, 2).await;
    let before: Vec<Value> = sqlx::query_scalar("SELECT to_jsonb(o) FROM operations o ORDER BY id")
        .fetch_all(&pool)
        .await
        .unwrap();
    sandbox_store::MIGRATOR.run(&pool).await.unwrap();
    let mut after: Vec<Value> =
        sqlx::query_scalar("SELECT to_jsonb(o) FROM operations o ORDER BY id")
            .fetch_all(&pool)
            .await
            .unwrap();
    for row in &mut after {
        for key in [
            "payload_compacted_at",
            "command_summary",
            "payload_compaction_next_at",
            "file_allocation_id",
        ] {
            assert_eq!(row.as_object_mut().unwrap().remove(key), Some(Value::Null));
        }
    }
    assert_eq!(before, after);
    assert_eq!(
        Store::from_pool(pool)
            .assign_response_retention(policy(60))
            .await
            .unwrap(),
        2
    );
}

struct NoStorage;
impl sandbox_cleanup::Retirement for NoStorage {
    async fn retire(
        &self,
        _: &sandbox_protocol::output::OutputPlan,
        _: &sandbox_protocol::output::OutputOwner,
        _: Option<&sandbox_protocol::output::OutputRef>,
        _: i64,
    ) -> Result<sandbox_protocol::output::OutputRetirement, sandbox_artifacts::Error> {
        panic!("response retention grants no storage authority")
    }
}
#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn cleanup_assigns_response_retention_only_with_explicit_operator_policy(pool: PgPool) {
    use sandbox_cleanup::{Cleaner, CleanupTick};
    seed(&pool, 1).await;
    let store = Store::from_pool(pool.clone());
    let before = snapshot(&pool).await;
    let worker = Cleaner::new(store.clone(), NoStorage, false);
    assert_eq!(worker.tick().await.unwrap(), CleanupTick::Idle);
    let unset: bool = sqlx::query_scalar("SELECT response_expires_at IS NULL FROM operations")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(unset);
    let worker = Cleaner::new(store, NoStorage, false).with_response_retention(policy(60));
    assert_eq!(
        worker.tick().await.unwrap(),
        CleanupTick::RetentionAssigned(1)
    );
    assert_eq!(worker.tick().await.unwrap(), CleanupTick::Idle);
    assert_eq!(snapshot(&pool).await, before);
}
