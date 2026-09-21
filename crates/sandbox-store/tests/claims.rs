//! PostgreSQL concurrency and recovery tests, each in its own database.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use sandbox_protocol::{Id, OperationId, ProjectId, SandboxId};
use sandbox_store::{
    Store,
    claims::{ClaimError, OperationKind},
};
use serde_json::json;
use sqlx::PgPool;

async fn operation(pool: &PgPool, kind: &str) -> OperationId {
    let project = ProjectId::generate();
    let sandbox = SandboxId::generate();
    let operation = OperationId::generate();
    sqlx::query("INSERT INTO projects(id,name,status,limits) VALUES($1,'claims','active','{}')")
        .bind(project.uuid())
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO sandboxes(id,project_id,image_digest,resources,desired_state,observed_state) VALUES($1,$2,'test','{}','running','creating')")
        .bind(sandbox.uuid()).bind(project.uuid()).execute(pool).await.unwrap();
    sqlx::query("INSERT INTO operations(id,project_id,sandbox_id,kind,initiator_kind,idempotency_key,request_digest,digest_version,payload,status) VALUES($1,$2,$3,$4,'service','claims-test-key-01','\\x00',1,'{}','queued')")
        .bind(operation.uuid()).bind(project.uuid()).bind(sandbox.uuid()).bind(kind).execute(pool).await.unwrap();
    operation
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn concurrent_controllers_claim_once(pool: PgPool) {
    let id = operation(&pool, "create").await;
    let store = Store::from_pool(pool.clone());
    let mut tasks = tokio::task::JoinSet::new();
    for _ in 0..8 {
        let store = store.clone();
        tasks.spawn(async move { store.claim_next(OperationKind::Create, 30).await });
    }
    let mut claims = Vec::new();
    while let Some(result) = tasks.join_next().await {
        if let Some(claim) = result.unwrap().unwrap() {
            claims.push(claim);
        }
    }
    assert_eq!(claims.len(), 1);
    assert_eq!(claims[0].operation_id, id);
    assert_eq!(claims[0].revision, 1);
    assert_eq!(claims[0].previous_status, "queued");
    let (attempts,): (i32,) = sqlx::query_as("SELECT attempt_count FROM operations WHERE id=$1")
        .bind(id.uuid())
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(attempts, 0, "claiming must not pretend a dispatch happened");
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn locked_work_does_not_block_other_work(pool: PgPool) {
    let first = operation(&pool, "create").await;
    let second = operation(&pool, "create").await;
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("SELECT id FROM operations WHERE id=$1 FOR UPDATE")
        .bind(first.uuid())
        .execute(&mut *tx)
        .await
        .unwrap();
    let store = Store::from_pool(pool.clone());
    let claim = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        store.claim_next(OperationKind::Create, 30),
    )
    .await
    .expect("must skip locked row")
    .unwrap()
    .unwrap();
    assert_eq!(claim.operation_id, second);
    tx.rollback().await.unwrap();
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn expired_claim_cannot_mutate_even_before_replacement(pool: PgPool) {
    operation(&pool, "create").await;
    let store = Store::from_pool(pool.clone());
    let stale = store
        .claim_next(OperationKind::Create, 30)
        .await
        .unwrap()
        .unwrap();
    sqlx::query("UPDATE operations SET lease_expires_at=now()-interval '1 second'")
        .execute(&pool)
        .await
        .unwrap();
    assert!(matches!(
        store.renew_claim(&stale, 30).await,
        Err(ClaimError::LostClaim)
    ));
    assert!(matches!(
        store.defer_claim(&stale, 1).await,
        Err(ClaimError::LostClaim)
    ));
    let replacement = store
        .claim_next(OperationKind::Create, 30)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(replacement.operation_id, stale.operation_id);
    assert_eq!(replacement.revision, stale.revision + 1);
    assert!(matches!(
        store.renew_claim(&stale, 30).await,
        Err(ClaimError::LostClaim)
    ));
    assert!(matches!(
        store.defer_claim(&stale, 1).await,
        Err(ClaimError::LostClaim)
    ));
    store.renew_claim(&replacement, 60).await.unwrap();
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn unknown_outcome_and_receipts_survive_reclaim(pool: PgPool) {
    let id = operation(&pool, "create").await;
    let receipts = json!([{"phase":"dispatch_intent","generation":1}]);
    sqlx::query("UPDATE operations SET status='unknown',phase='readiness_unknown',attempt_receipts=$1,deadline=now()-interval '1 hour',claim_revision=4 WHERE id=$2")
        .bind(&receipts).bind(id.uuid()).execute(&pool).await.unwrap();
    let store = Store::from_pool(pool.clone());
    let claim = store
        .claim_next(OperationKind::Create, 30)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(claim.previous_status, "unknown");
    assert_eq!(claim.phase.as_deref(), Some("readiness_unknown"));
    assert_eq!(claim.receipts, receipts);
    assert_eq!(claim.revision, 5);
    assert!(
        claim.deadline.is_some(),
        "expired work still needs reconciliation"
    );
    let (status,): (String,) = sqlx::query_as("SELECT status FROM operations WHERE id=$1")
        .bind(id.uuid())
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(status, "unknown");
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn deferral_respects_retry_time_and_invalidates_owner(pool: PgPool) {
    operation(&pool, "create").await;
    let store = Store::from_pool(pool.clone());
    let first = store
        .claim_next(OperationKind::Create, 30)
        .await
        .unwrap()
        .unwrap();
    store.defer_claim(&first, 30).await.unwrap();
    assert!(
        store
            .claim_next(OperationKind::Create, 30)
            .await
            .unwrap()
            .is_none()
    );
    assert!(matches!(
        store.renew_claim(&first, 30).await,
        Err(ClaimError::LostClaim)
    ));
    sqlx::query("UPDATE operations SET next_retry_at=now()-interval '1 second'")
        .execute(&pool)
        .await
        .unwrap();
    let next = store
        .claim_next(OperationKind::Create, 30)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(next.revision, first.revision + 1);
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn terminal_and_unsupported_work_is_not_claimed(pool: PgPool) {
    operation(&pool, "execute").await;
    let finished = operation(&pool, "create").await;
    sqlx::query("UPDATE operations SET status='succeeded',completed_at=now() WHERE id=$1")
        .bind(finished.uuid())
        .execute(&pool)
        .await
        .unwrap();
    let store = Store::from_pool(pool);
    assert!(
        store
            .claim_next(OperationKind::Create, 30)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .claim_next(OperationKind::Execute, 30)
            .await
            .unwrap()
            .is_some()
    );
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn lease_and_retry_bounds_are_validated_before_mutation(pool: PgPool) {
    operation(&pool, "create").await;
    let store = Store::from_pool(pool);
    for seconds in [0, 301, u32::MAX] {
        assert!(matches!(
            store.claim_next(OperationKind::Create, seconds).await,
            Err(ClaimError::InvalidLease)
        ));
    }
    let claim = store
        .claim_next(OperationKind::Create, 30)
        .await
        .unwrap()
        .unwrap();
    for seconds in [0, 3601, u32::MAX] {
        assert!(matches!(
            store.defer_claim(&claim, seconds).await,
            Err(ClaimError::InvalidRetryDelay)
        ));
    }
    assert_eq!(claim.revision, 1);
    store.renew_claim(&claim, 30).await.unwrap();
}
