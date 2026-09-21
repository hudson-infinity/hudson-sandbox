use super::*;
use sandbox_protocol::supervisor::CommandObservation;
use sandbox_store::cancel::{CancelAdmission, CancelCommand, CancelProgress};
fn request(f: &Fixture, target: OperationId) -> CancelCommand {
    CancelCommand {
        project_id: f.request.project_id,
        target,
        key_id: f.request.key_id.clone(),
        idempotency_key: key(),
    }
}
async fn admit(f: &Fixture, r: &CancelCommand) -> OperationId {
    let CancelAdmission::Accepted { operation_id, .. } = f.store.admit_cancel(r).await.unwrap()
    else {
        panic!("cancel admission")
    };
    operation_id
}
async fn claim(f: &Fixture) -> Claim {
    f.store
        .claim_next(OperationKind::Cancel, 30)
        .await
        .unwrap()
        .unwrap()
}
async fn read(f: &Fixture, id: OperationId) -> serde_json::Value {
    sqlx::query_scalar("SELECT to_jsonb(o) FROM operations o WHERE id=$1")
        .bind(id.uuid())
        .fetch_one(f.store.pool())
        .await
        .unwrap()
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn cancel_admission_converges_and_distinct_pending_requests_conflict(pool: PgPool) {
    let f = Fixture::new(&pool).await;
    let target = f.admit().await;
    let r = request(&f, target);
    let mut tasks = tokio::task::JoinSet::new();
    for _ in 0..8 {
        let store = f.store.clone();
        let r = r.clone();
        tasks.spawn(async move { store.admit_cancel(&r).await.unwrap() });
    }
    let mut ids = std::collections::BTreeSet::new();
    while let Some(result) = tasks.join_next().await {
        let CancelAdmission::Accepted { operation_id, .. } = result.unwrap() else {
            panic!("retry")
        };
        ids.insert(operation_id);
    }
    assert_eq!(ids.len(), 1);
    let id = *ids.first().unwrap();
    assert_eq!(
        f.store.admit_cancel(&request(&f, target)).await.unwrap(),
        CancelAdmission::Busy(id)
    );
    let mut changed = r;
    changed.target = OperationId::generate();
    assert_eq!(
        f.store.admit_cancel(&changed).await.unwrap(),
        CancelAdmission::DigestConflict
    );
    let row = read(&f, target).await;
    assert_eq!(row["status"], "queued");
    assert_eq!(row["attempt_count"], 0);
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn queued_cancellation_has_no_dispatch_and_compaction_keeps_retry_identity(pool: PgPool) {
    let f = Fixture::new(&pool).await;
    let target = f.admit().await;
    let r = request(&f, target);
    let id = admit(&f, &r).await;
    let before: serde_json::Value =
        sqlx::query_scalar("SELECT to_jsonb(a) FROM allocations a WHERE id=$1")
            .bind(f.allocation.uuid())
            .fetch_one(&pool)
            .await
            .unwrap();
    let execution = f.claim().await;
    assert!(matches!(
        f.store
            .prepare_execute(&execution, f.host, 1)
            .await
            .unwrap(),
        ExecuteAction::Rejected
    ));
    let row = read(&f, target).await;
    assert_eq!(row["status"], "cancelled");
    assert_eq!(row["attempt_count"], 0);
    assert_eq!(row["attempt_receipts"], json!([]));
    assert_eq!(row["result"]["dispatch_intent_absent"], true);
    assert_eq!(
        f.store.reconcile_cancel(&claim(&f).await).await.unwrap(),
        CancelProgress::Completed
    );
    let cancelled = read(&f, id).await;
    assert_eq!(
        cancelled["result"],
        json!({"target_operation_id":target.to_string(),"target_status":"cancelled","cancelled":true})
    );
    let after: serde_json::Value =
        sqlx::query_scalar("SELECT to_jsonb(a) FROM allocations a WHERE id=$1")
            .bind(f.allocation.uuid())
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(before, after);
    sqlx::query("UPDATE operations SET response_expires_at=clock_timestamp()-interval '1 second' WHERE id=$1")
        .bind(id.uuid()).execute(&pool).await.unwrap();
    assert_eq!(
        f.store.compact_expired_response().await.unwrap(),
        sandbox_store::compaction::Compaction::Completed(id)
    );
    assert_eq!(
        f.store.admit_cancel(&r).await.unwrap(),
        CancelAdmission::ResponseExpired(id)
    );
    assert_eq!(
        read(&f, id).await["target_operation_id"],
        json!(target.uuid())
    );
    assert_eq!(read(&f, target).await, row);
    let _ = f.destroy().await;
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn unknown_execution_is_cancelled_under_original_ownership_even_after_revocation(
    pool: PgPool,
) {
    let f = Fixture::new(&pool).await;
    let target = f.admit().await;
    let execution = f.claim().await;
    let ExecuteAction::Dispatch { owner, command } = f
        .store
        .prepare_execute(&execution, f.host, 1)
        .await
        .unwrap()
    else {
        panic!("dispatch")
    };
    f.store.record_execute_unknown(&execution).await.unwrap();
    let r = request(&f, target);
    let id = admit(&f, &r).await;
    assert_eq!(
        f.store.reconcile_cancel(&claim(&f).await).await.unwrap(),
        CancelProgress::Pending
    );
    assert_eq!(read(&f, target).await["status"], "unknown");
    sqlx::query("UPDATE projects SET api_tokens='[]' WHERE id=$1")
        .bind(f.request.project_id.uuid())
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "UPDATE operations SET next_retry_at=clock_timestamp()-interval '1 second' WHERE id=$1",
    )
    .bind(target.uuid())
    .execute(&pool)
    .await
    .unwrap();
    let execution = f.claim().await;
    let ExecuteAction::Cancel {
        owner: cancel_owner,
        digest,
    } = f
        .store
        .prepare_execute(&execution, f.host, 1)
        .await
        .unwrap()
    else {
        panic!("cancel")
    };
    assert_eq!(cancel_owner.allocation_id, owner.allocation_id);
    assert_eq!(cancel_owner.operation_id, owner.operation_id);
    assert_eq!(digest, command.digest().unwrap());
    assert_eq!(read(&f, target).await["attempt_count"], 1);
    let observation = CommandObservation {
        ownership: Some(cancel_owner),
        simulated: true,
        observed_unix_ms: now_ms(),
        command_digest: digest.to_vec(),
        receipt: None,
        not_started: true,
    };
    f.store
        .record_execute_observation(&execution, &observation, true)
        .await
        .unwrap();
    assert_eq!(read(&f, target).await["status"], "cancelled");
    sqlx::query(
        "UPDATE operations SET next_retry_at=clock_timestamp()-interval '1 second' WHERE id=$1",
    )
    .bind(id.uuid())
    .execute(&pool)
    .await
    .unwrap();
    assert_eq!(
        f.store.reconcile_cancel(&claim(&f).await).await.unwrap(),
        CancelProgress::Completed
    );
    assert_eq!(read(&f, id).await["result"]["cancelled"], true);
    assert_eq!(
        f.store.admit_cancel(&r).await.unwrap(),
        CancelAdmission::Unauthorized
    );
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn terminal_target_is_an_explicit_noop_and_scope_is_checked(pool: PgPool) {
    let f = Fixture::new(&pool).await;
    let target = f.admit().await;
    sqlx::query("UPDATE operations SET status='succeeded',completed_at=clock_timestamp(),result='{}' WHERE id=$1").bind(target.uuid()).execute(&pool).await.unwrap();
    let before = read(&f, target).await;
    let r = request(&f, target);
    let id = admit(&f, &r).await;
    let row = read(&f, id).await;
    assert_eq!(row["status"], "succeeded");
    assert_eq!(row["result"]["cancelled"], false);
    assert_eq!(row["result"]["target_status"], "succeeded");
    assert_eq!(read(&f, target).await, before);
    assert_eq!(
        f.store.admit_cancel(&request(&f, id)).await.unwrap(),
        CancelAdmission::Unsupported
    );
    let other = Fixture::new(&pool).await;
    assert_eq!(
        other
            .store
            .admit_cancel(&request(&other, target))
            .await
            .unwrap(),
        CancelAdmission::NotFound
    );
    assert_eq!(
        other
            .store
            .admit_cancel(&request(&other, OperationId::generate()))
            .await
            .unwrap(),
        CancelAdmission::NotFound
    );
    let cancel_view = f
        .store
        .operation_for_project(f.request.project_id, id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(cancel_view.target_operation_id, Some(target));
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn cancel_completion_rechecks_its_claim_after_target_lock_wait(pool: PgPool) {
    let f = Fixture::new(&pool).await;
    let target = f.admit().await;
    let id = admit(&f, &request(&f, target)).await;
    let claim = f
        .store
        .claim_next(OperationKind::Cancel, 1)
        .await
        .unwrap()
        .unwrap();
    let mut lock = pool.begin().await.unwrap();
    sqlx::query("SELECT id FROM operations WHERE id=$1 FOR UPDATE")
        .bind(target.uuid())
        .execute(&mut *lock)
        .await
        .unwrap();
    let store = f.store.clone();
    let task = tokio::spawn(async move { store.reconcile_cancel(&claim).await });
    tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
    lock.commit().await.unwrap();
    assert!(matches!(task.await.unwrap(), Err(DispatchError::LostClaim)));
    let row = read(&f, id).await;
    assert_eq!(row["status"], "running");
    assert!(row["result"].is_null());
    assert_eq!(read(&f, target).await["status"], "queued");
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn abandoned_admission_rolls_back_and_retry_can_still_cancel(pool: PgPool) {
    let f = Fixture::new(&pool).await;
    let target = f.admit().await;
    let r = request(&f, target);
    let mut lock = pool.begin().await.unwrap();
    sqlx::query("SELECT id FROM operations WHERE id=$1 FOR UPDATE")
        .bind(target.uuid())
        .execute(&mut *lock)
        .await
        .unwrap();
    let store = f.store.clone();
    let abandoned = r.clone();
    let task = tokio::spawn(async move { store.admit_cancel(&abandoned).await });
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    lock.commit().await.unwrap();
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM operations WHERE kind='cancel'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
    let _ = admit(&f, &r).await;
    assert_eq!(
        f.store.reconcile_cancel(&claim(&f).await).await.unwrap(),
        CancelProgress::Completed
    );
    assert_eq!(read(&f, target).await["status"], "cancelled");
}

#[sqlx::test(migrations = false)]
async fn cancellation_upgrade_preserves_generic_rows_without_activating_them(pool: PgPool) {
    let old = sqlx::migrate::Migrator {
        migrations: std::borrow::Cow::Owned(
            sandbox_store::MIGRATOR.iter().take(11).cloned().collect(),
        ),
        ..sqlx::migrate::Migrator::DEFAULT
    };
    old.run(&pool).await.unwrap();
    let mut f = Fixture::new(&pool).await;
    let target = f.admit().await;
    let legacy = OperationId::generate();
    sqlx::query("INSERT INTO operations(id,project_id,sandbox_id,kind,initiator_kind,idempotency_key,request_digest,digest_version,payload,target_operation_id,status)
        SELECT $1,project_id,sandbox_id,'cancel','service',$2,request_digest,digest_version,'{}',id,'queued' FROM operations WHERE id=$3")
        .bind(legacy.uuid()).bind(key().as_str()).bind(target.uuid()).execute(&pool).await.unwrap();
    let before = read(&f, legacy).await;
    sandbox_store::MIGRATOR.run(&pool).await.unwrap();
    let pool = reconnect_after_upgrade(&pool).await;
    f.store = Store::from_pool(pool);
    let mut after = read(&f, legacy).await;
    assert_eq!(
        after.as_object_mut().unwrap().remove("file_allocation_id"),
        Some(serde_json::Value::Null)
    );
    assert_eq!(after, before);
    assert!(
        f.store
            .claim_next(OperationKind::Cancel, 30)
            .await
            .unwrap()
            .is_none()
    );
    let execution = f.claim().await;
    assert!(matches!(
        f.store
            .prepare_execute(&execution, f.host, 1)
            .await
            .unwrap(),
        ExecuteAction::Dispatch { .. }
    ));
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn contradictory_dispatch_history_cannot_be_declared_cancelled_without_rpc(pool: PgPool) {
    let f = Fixture::new(&pool).await;
    let target = f.admit().await;
    let execution = f.claim().await;
    sqlx::query("UPDATE operations SET attempt_receipts=$2 WHERE id=$1")
        .bind(target.uuid())
        .bind(json!([{"phase":"execute_dispatch_intent"}]))
        .execute(&pool)
        .await
        .unwrap();
    let id = admit(&f, &request(&f, target)).await;
    assert_eq!(
        f.store.reconcile_cancel(&claim(&f).await).await.unwrap(),
        CancelProgress::Pending
    );
    assert!(matches!(
        f.store.prepare_execute(&execution, f.host, 1).await,
        Err(DispatchError::InvalidData)
    ));
    assert_eq!(read(&f, target).await["status"], "running");
    assert_ne!(read(&f, id).await["status"], "succeeded");
}
