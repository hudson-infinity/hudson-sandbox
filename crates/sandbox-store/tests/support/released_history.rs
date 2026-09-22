//! Synthetic proof fixtures test database validation, not physical destruction.
use super::*;
use sandbox_protocol::supervisor::{
    AllocationState, ReleasedHistoryObservation, ReleasedHistoryRequest,
};
use sandbox_store::history::ReleasedPreparation;
async fn release(f: &Fixture) {
    sqlx::query("UPDATE allocations SET status='released',released_at=clock_timestamp(),release_evidence='{}' WHERE id=$1").bind(f.allocation.uuid()).execute(f.store.pool()).await.unwrap();
    sqlx::query("UPDATE sandboxes SET desired_state='destroyed',observed_state='destroyed',destroyed_at=clock_timestamp(),current_allocation_id=NULL WHERE id=$1").bind(f.request.sandbox_id.uuid()).execute(f.store.pool()).await.unwrap();
}
async fn claim(f: &Fixture, epoch: i64) -> ReleasedPreparation {
    f.store
        .claim_released_history(f.host, epoch, Domain::Commands, 30, false)
        .await
        .unwrap()
        .unwrap()
}
fn observed(request: ReleasedHistoryRequest) -> ReleasedHistoryObservation {
    ReleasedHistoryObservation {
        completed_through: request.through.clone(),
        request: Some(request),
        release_state: AllocationState::Released as i32,
        simulated: false,
        observed_unix_ms: now_ms(),
    }
}
async fn count(f: &Fixture) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM completed_allocation_history WHERE allocation_id=$1")
        .bind(f.allocation.uuid())
        .fetch_one(f.store.pool())
        .await
        .unwrap()
}
#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn released_history_requires_exact_destruction_proof_and_preserves_original_retries(
    pool: PgPool,
) {
    let f = Fixture::new(&pool).await;
    let id = f.admit().await;
    finish(&f, id).await;
    assert!(
        f.store
            .claim_released_history(f.host, 1, Domain::Commands, 30, false)
            .await
            .unwrap()
            .is_none()
    );
    release(&f).await;
    let p = claim(&f, 1).await;
    assert_eq!(count(&f).await, 0);
    let before: serde_json::Value =
        sqlx::query_scalar("SELECT to_jsonb(o) FROM operations o WHERE id=$1")
            .bind(id.uuid())
            .fetch_one(&pool)
            .await
            .unwrap();
    for n in 0..7 {
        let mut bad = observed(p.request.clone());
        match n {
            0 => bad.completed_through = OperationId::generate().to_string(),
            1 => bad.request.as_mut().unwrap().reporting_epoch += 1,
            2 => {
                bad.request
                    .as_mut()
                    .unwrap()
                    .ownership
                    .as_mut()
                    .unwrap()
                    .generation += 1
            }
            3 => bad.release_state = AllocationState::Absent as i32,
            4 => bad.simulated = true,
            5 => bad.observed_unix_ms -= 20_000,
            _ => bad.request.as_mut().unwrap().domain = 2,
        }
        assert!(matches!(
            f.store
                .complete_released_history(&p.claim, &bad, false)
                .await,
            Err(Error::Evidence)
        ));
        assert_eq!(count(&f).await, 0);
    }
    f.store
        .complete_released_history(&p.claim, &observed(p.request), false)
        .await
        .unwrap();
    assert_eq!(count(&f).await, 1);
    let after: serde_json::Value =
        sqlx::query_scalar("SELECT to_jsonb(o) FROM operations o WHERE id=$1")
            .bind(id.uuid())
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(before, after);
    assert!(
        matches!(f.store.admit_execute(&f.request).await.unwrap(),ExecuteAdmission::Accepted {operation_id,..} if operation_id==id)
    );
    sqlx::query("UPDATE released_allocation_history SET completion=jsonb_set(completion,'{release_state}','1') WHERE allocation_id=$1").bind(f.allocation.uuid()).execute(&pool).await.unwrap();
    assert_eq!(count(&f).await, 0);
}
#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn released_history_lost_reply_keeps_prefix_and_original_epoch(pool: PgPool) {
    let mut f = Fixture::new(&pool).await;
    let first = f.admit().await;
    finish(&f, first).await;
    f.request.idempotency_key = key();
    let second = f.admit().await;
    release(&f).await;
    let old = claim(&f, 1).await;
    assert_eq!(old.request.through, first.to_string());
    finish(&f, second).await;
    sqlx::query("UPDATE released_allocation_history SET lease_expires_at=clock_timestamp()-interval '1 second' WHERE allocation_id=$1").bind(f.allocation.uuid()).execute(&pool).await.unwrap();
    sqlx::query("UPDATE hosts SET supervisor_epoch=2 WHERE id=$1")
        .bind(f.host.uuid())
        .execute(&pool)
        .await
        .unwrap();
    let new = claim(&f, 2).await;
    assert_eq!(new.request.through, old.request.through);
    assert_eq!(new.request.reporting_epoch, 2);
    assert_eq!(new.request.ownership.as_ref().unwrap().supervisor_epoch, 1);
    assert!(new.claim.claim.revision > old.claim.claim.revision);
    assert!(matches!(
        f.store
            .complete_released_history(&old.claim, &observed(old.request), false)
            .await,
        Err(Error::LostClaim)
    ));
    f.store
        .complete_released_history(&new.claim, &observed(new.request), false)
        .await
        .unwrap();
    let next = claim(&f, 2).await;
    assert_eq!(next.request.through, second.to_string());
    assert_eq!(count(&f).await, 1); // The previous verified prefix remains credited.
    f.store
        .complete_released_history(&next.claim, &observed(next.request), false)
        .await
        .unwrap();
    let prefix: uuid::Uuid = sqlx::query_scalar(
        "SELECT completed_through FROM completed_allocation_history WHERE allocation_id=$1",
    )
    .bind(f.allocation.uuid())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(prefix, second.uuid());
}
#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn released_history_unknown_predecessor_and_epoch_change_never_refund(pool: PgPool) {
    let mut f = Fixture::new(&pool).await;
    let first = f.admit().await;
    finish(&f, first).await;
    f.request.idempotency_key = key();
    let second = f.admit().await;
    finish(&f, second).await;
    sqlx::query("UPDATE operations SET status='unknown',completed_at=NULL WHERE id=$1")
        .bind(first.uuid())
        .execute(&pool)
        .await
        .unwrap();
    release(&f).await;
    assert!(
        f.store
            .claim_released_history(f.host, 1, Domain::Commands, 30, false)
            .await
            .unwrap()
            .is_none()
    );
    finish(&f, first).await;
    let p = claim(&f, 1).await;
    assert_eq!(p.request.through, second.to_string());
    sqlx::query("UPDATE operations SET status='unknown',completed_at=NULL WHERE id=$1")
        .bind(second.uuid())
        .execute(&pool)
        .await
        .unwrap();
    assert!(matches!(
        f.store
            .complete_released_history(&p.claim, &observed(p.request.clone()), false)
            .await,
        Err(Error::Evidence)
    ));
    finish(&f, second).await;
    sqlx::query("UPDATE hosts SET supervisor_epoch=2 WHERE id=$1")
        .bind(f.host.uuid())
        .execute(&pool)
        .await
        .unwrap();
    assert!(matches!(
        f.store
            .complete_released_history(&p.claim, &observed(p.request), false)
            .await,
        Err(Error::LostClaim)
    ));
    assert_eq!(count(&f).await, 0);
}
#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn released_history_extends_verified_live_prefix_without_double_credit(pool: PgPool) {
    let mut f = Fixture::new(&pool).await;
    let first = f.admit().await;
    finish(&f, first).await;
    let (c, r) = prepare(&f).await;
    f.store
        .complete_history(&c, &observation(r), false)
        .await
        .unwrap();
    f.request.idempotency_key = key();
    let second = f.admit().await;
    finish(&f, second).await;
    release(&f).await;
    let p = claim(&f, 1).await;
    assert_eq!(p.request.through, second.to_string());
    assert_eq!(count(&f).await, 1);
    f.store
        .complete_released_history(&p.claim, &observed(p.request), false)
        .await
        .unwrap();
    assert_eq!(count(&f).await, 1);
    let prefix: uuid::Uuid = sqlx::query_scalar(
        "SELECT completed_through FROM completed_allocation_history WHERE allocation_id=$1",
    )
    .bind(f.allocation.uuid())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(prefix, second.uuid());
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn released_history_expired_claim_after_allocation_lock_wait_cannot_complete(pool: PgPool) {
    let f = Fixture::new(&pool).await;
    let id = f.admit().await;
    finish(&f, id).await;
    release(&f).await;
    let p = f
        .store
        .claim_released_history(f.host, 1, Domain::Commands, 1, false)
        .await
        .unwrap()
        .unwrap();
    let mut held = pool.begin().await.unwrap();
    sqlx::query("SELECT id FROM allocations WHERE id=$1 FOR UPDATE")
        .bind(f.allocation.uuid())
        .fetch_one(&mut *held)
        .await
        .unwrap();
    let store = f.store.clone();
    let job = tokio::spawn(async move {
        store
            .complete_released_history(&p.claim, &observed(p.request), false)
            .await
    });
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    held.commit().await.unwrap();
    assert!(matches!(job.await.unwrap(), Err(Error::LostClaim)));
    assert_eq!(count(&f).await, 0);
    let retry = claim(&f, 1).await;
    f.store
        .complete_released_history(&retry.claim, &observed(retry.request), false)
        .await
        .unwrap();
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn released_history_corrupt_live_prefix_cannot_skip_old_operations(pool: PgPool) {
    let mut f = Fixture::new(&pool).await;
    let first = f.admit().await;
    finish(&f, first).await;
    let (c, r) = prepare(&f).await;
    f.store
        .complete_history(&c, &observation(r), false)
        .await
        .unwrap();
    f.request.idempotency_key = key();
    let second = f.admit().await;
    finish(&f, second).await;
    release(&f).await;
    sqlx::query("UPDATE allocation_history SET completion=jsonb_set(completion,'{completed,through}',to_jsonb($2::text)) WHERE allocation_id=$1").bind(f.allocation.uuid()).bind(OperationId::generate().to_string()).execute(&pool).await.unwrap();
    assert!(
        f.store
            .claim_released_history(f.host, 1, Domain::Commands, 30, false)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(count(&f).await, 0);
}

#[sqlx::test(migrations = false)]
async fn released_history_upgrade_preserves_live_proof_and_original_rows(pool: PgPool) {
    let old = sqlx::migrate::Migrator {
        migrations: std::borrow::Cow::Owned(
            sandbox_store::MIGRATOR.iter().take(15).cloned().collect(),
        ),
        ..sqlx::migrate::Migrator::DEFAULT
    };
    old.run(&pool).await.unwrap();
    let mut f = Fixture::new(&pool).await;
    let first = legacy_execute::seed(&pool, &f.request, f.allocation).await;
    finish(&f, first).await;
    let (c, r) = prepare(&f).await;
    f.store
        .complete_history(&c, &observation(r), false)
        .await
        .unwrap();
    let before: serde_json::Value =
        sqlx::query_scalar("SELECT to_jsonb(h) FROM allocation_history h")
            .fetch_one(&pool)
            .await
            .unwrap();
    sandbox_store::MIGRATOR.run(&pool).await.unwrap();
    let pool = reconnect_after_upgrade(&pool).await;
    f.store = Store::from_pool(pool.clone());
    let after: serde_json::Value =
        sqlx::query_scalar("SELECT to_jsonb(h) FROM allocation_history h")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(before, after);
    assert_eq!(count(&f).await, 1);
    f.request.idempotency_key = key();
    let second = f.admit().await;
    finish(&f, second).await;
    release(&f).await;
    let p = claim(&f, 1).await;
    f.store
        .complete_released_history(&p.claim, &observed(p.request), false)
        .await
        .unwrap();
    assert_eq!(count(&f).await, 1);
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM operations")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(n, 2);
}
