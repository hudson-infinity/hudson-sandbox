use super::*;
use sandbox_store::compaction::Compaction;

async fn expire_response(f: &Fixture) {
    sqlx::query("UPDATE operations SET response_expires_at=clock_timestamp()-interval '1 second' WHERE id=$1")
        .bind(f.operation.uuid()).execute(f.store.pool()).await.unwrap();
}
async fn row(f: &Fixture) -> Value {
    sqlx::query_scalar("SELECT to_jsonb(o) FROM operations o WHERE id=$1")
        .bind(f.operation.uuid())
        .fetch_one(f.store.pool())
        .await
        .unwrap()
}
fn evidence(mut value: Value) -> Value {
    for key in [
        "payload",
        "result",
        "error",
        "payload_compacted_at",
        "command_summary",
        "payload_compaction_next_at",
        "output_status",
        "output_claim_revision",
        "output_lease_expires_at",
        "output_next_retry_at",
        "output_expires_at",
    ] {
        value.as_object_mut().unwrap().remove(key);
    }
    value
}
async fn pending(pool: &PgPool) -> Fixture {
    let f = Fixture::new(pool).await;
    f.finish().await;
    expire_response(&f).await;
    f
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn compaction_fences_unissued_output_and_retains_execution_and_retry_evidence(pool: PgPool) {
    let f = pending(&pool).await;
    let old = f.store.claim_output(30).await.unwrap().unwrap();
    let before = row(&f).await;
    assert_eq!(
        f.store.compact_expired_response().await.unwrap(),
        Compaction::Completed(f.operation)
    );
    let after = row(&f).await;
    assert_eq!(after["payload"], json!({}));
    assert!(after["result"].is_null() && after["error"].is_null());
    assert!(after["payload_compacted_at"].is_string());
    assert_eq!(after["output_status"], "expired");
    assert!(after["output_ticket"].is_null());
    assert_eq!(evidence(after.clone()), evidence(before));
    assert!(!after.to_string().contains("private-command"));
    assert!(matches!(
        f.store.prepare_output(&old, 3600, 0, true).await,
        Err(OutputError::LostClaim)
    ));
    assert!(f.store.claim_output(30).await.unwrap().is_none());
    assert_eq!(f.store.enqueue_expired_output(100).await.unwrap(), 0);
    assert!(
        f.store
            .claim_next(OperationKind::Execute, 30)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        f.store.compact_expired_response().await.unwrap(),
        Compaction::Idle
    );
    assert_eq!(row(&f).await, after);
    let view = f
        .store
        .output_for_project(f.project, f.operation)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(view.status, "expired");
    assert!(view.references.is_none());
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn compaction_requires_completed_retirement_and_cleanup_ack_retries_still_verify(
    pool: PgPool,
) {
    let (f, c, receipt) = prepared(&pool).await;
    expire_response(&f).await;
    let before = row(&f).await;
    assert_eq!(
        f.store.compact_expired_response().await.unwrap(),
        Compaction::Idle
    );
    f.store
        .complete_output_cleanup(&c, &receipt, true)
        .await
        .unwrap();
    assert_eq!(
        f.store.compact_expired_response().await.unwrap(),
        Compaction::Completed(f.operation)
    );
    assert_eq!(evidence(row(&f).await), evidence(before));
    f.store
        .complete_output_cleanup(&c, &receipt, true)
        .await
        .unwrap();
    assert!(matches!(
        f.store.complete_output_cleanup(&c, &receipt, false).await,
        Err(OutputError::SimulationDenied)
    ));
    assert_eq!(
        f.store
            .output_for_project(f.project, f.operation)
            .await
            .unwrap()
            .unwrap()
            .status,
        "expired"
    );
    sqlx::query("UPDATE operations SET command_summary=jsonb_set(command_summary,'{output_limit}','999') WHERE id=$1")
        .bind(f.operation.uuid()).execute(&pool).await.unwrap();
    assert!(matches!(
        f.store.complete_output_cleanup(&c, &receipt, true).await,
        Err(OutputError::Corrupt)
    ));
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn compaction_defers_corrupt_request_or_retirement_without_erasing_evidence(pool: PgPool) {
    let (f, c, receipt) = prepared(&pool).await;
    f.store
        .complete_output_cleanup(&c, &receipt, true)
        .await
        .unwrap();
    expire_response(&f).await;
    sqlx::query("UPDATE output_cleanup SET receipt='{}' WHERE operation_id=$1")
        .bind(f.operation.uuid())
        .execute(&pool)
        .await
        .unwrap();
    let before = row(&f).await;
    assert_eq!(
        f.store.compact_expired_response().await.unwrap(),
        Compaction::Deferred(f.operation)
    );
    let after = row(&f).await;
    assert_eq!(after["payload"], before["payload"]);
    assert_eq!(after["result"], before["result"]);
    assert!(after["payload_compacted_at"].is_null());
    assert!(after["payload_compaction_next_at"].is_string());
    let g = pending(&pool).await;
    sqlx::query(
        "UPDATE operations SET payload=jsonb_set(payload,'{argv}','[\"tampered\"]') WHERE id=$1",
    )
    .bind(g.operation.uuid())
    .execute(&pool)
    .await
    .unwrap();
    assert_eq!(
        g.store.compact_expired_response().await.unwrap(),
        Compaction::Deferred(g.operation)
    );
    let h = pending(&pool).await;
    assert_eq!(
        h.store.compact_expired_response().await.unwrap(),
        Compaction::Completed(h.operation)
    );
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn compaction_preserves_unresolved_unexpired_and_authorized_upload_work(pool: PgPool) {
    let f = Fixture::new(&pool).await;
    expire_response(&f).await;
    let before = row(&f).await;
    assert_eq!(
        f.store.compact_expired_response().await.unwrap(),
        Compaction::Idle
    );
    assert_eq!(row(&f).await, before);
    sqlx::query("UPDATE operations SET status='unknown',lease_expires_at=NULL,next_retry_at=clock_timestamp()+interval '1 day' WHERE id=$1")
        .bind(f.operation.uuid())
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        f.store.compact_expired_response().await.unwrap(),
        Compaction::Idle
    );
    let g = Fixture::new(&pool).await;
    g.finish().await;
    let (_, work) = g.work().await;
    expire_response(&g).await;
    let before = row(&g).await;
    assert_eq!(
        g.store.compact_expired_response().await.unwrap(),
        Compaction::Idle
    );
    assert_eq!(row(&g).await, before);
    assert_eq!(before["output_ticket"], json!(work.ticket));
    let h = Fixture::new(&pool).await;
    h.finish().await;
    sqlx::query(
        "UPDATE operations SET response_expires_at=clock_timestamp()+interval '1 hour' WHERE id=$1",
    )
    .bind(h.operation.uuid())
    .execute(&pool)
    .await
    .unwrap();
    assert_eq!(
        h.store.compact_expired_response().await.unwrap(),
        Compaction::Idle
    );
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn compaction_schema_prevents_partial_or_resurrected_bodies(pool: PgPool) {
    let f = pending(&pool).await;
    let error =
        sqlx::query("UPDATE operations SET payload_compacted_at=clock_timestamp() WHERE id=$1")
            .bind(f.operation.uuid())
            .execute(&pool)
            .await
            .unwrap_err();
    assert_eq!(
        error.as_database_error().unwrap().code().as_deref(),
        Some("23514")
    );
    f.store.compact_expired_response().await.unwrap();
    for sql in [
        "UPDATE operations SET payload='{\"restored\":true}' WHERE id=$1",
        "UPDATE operations SET result='{}' WHERE id=$1",
        "UPDATE operations SET response_expires_at=clock_timestamp()+interval '1 day' WHERE id=$1",
        "UPDATE operations SET status='unknown',completed_at=NULL WHERE id=$1",
    ] {
        let error = sqlx::query(sql)
            .bind(f.operation.uuid())
            .execute(&pool)
            .await
            .unwrap_err();
        assert_eq!(
            error.as_database_error().unwrap().code().as_deref(),
            Some("23514")
        );
    }
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn concurrent_compactors_skip_locked_rows_and_never_duplicate_compaction(pool: PgPool) {
    let f = pending(&pool).await;
    let g = pending(&pool).await;
    let mut lock = pool.begin().await.unwrap();
    sqlx::query("SELECT id FROM operations WHERE id=$1 FOR UPDATE")
        .bind(f.operation.uuid())
        .fetch_one(&mut *lock)
        .await
        .unwrap();
    let (a, b) = tokio::join!(
        f.store.compact_expired_response(),
        g.store.compact_expired_response()
    );
    let results = [a.unwrap(), b.unwrap()];
    assert!(results.contains(&Compaction::Completed(g.operation)));
    assert!(results.contains(&Compaction::Idle));
    lock.rollback().await.unwrap();
    assert_eq!(
        f.store.compact_expired_response().await.unwrap(),
        Compaction::Completed(f.operation)
    );
}

#[sqlx::test(migrations = false)]
async fn compaction_upgrade_preserves_old_payload_and_then_reclaims_it(pool: PgPool) {
    let old = sqlx::migrate::Migrator {
        migrations: std::borrow::Cow::Owned(
            sandbox_store::MIGRATOR.iter().take(9).cloned().collect(),
        ),
        ..sqlx::migrate::Migrator::DEFAULT
    };
    old.run(&pool).await.unwrap();
    let f = pending(&pool).await;
    let before = row(&f).await;
    sandbox_store::MIGRATOR.run(&pool).await.unwrap();
    let mut after = row(&f).await;
    for key in [
        "payload_compacted_at",
        "command_summary",
        "payload_compaction_next_at",
        "file_allocation_id",
    ] {
        assert_eq!(
            after.as_object_mut().unwrap().remove(key),
            Some(Value::Null)
        );
    }
    assert_eq!(before, after);
    assert_eq!(
        f.store.compact_expired_response().await.unwrap(),
        Compaction::Completed(f.operation)
    );
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn compaction_worker_requires_explicit_opt_in_and_uses_no_storage_for_unissued_tickets(
    pool: PgPool,
) {
    let f = pending(&pool).await;
    let worker = Cleaner::new(f.store.clone(), NeverRetire, false);
    assert_eq!(worker.tick().await.unwrap(), CleanupTick::Idle);
    assert!(!row(&f).await["payload"].as_object().unwrap().is_empty());
    let worker = worker.with_payload_compaction();
    assert_eq!(
        worker.tick().await.unwrap(),
        CleanupTick::PayloadCompaction(Compaction::Completed(f.operation))
    );
    assert_eq!(worker.tick().await.unwrap(), CleanupTick::Idle);
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn compaction_cancellation_rolls_back_and_a_replacement_recovers(pool: PgPool) {
    use std::time::Duration;
    let (f, c, receipt) = prepared(&pool).await;
    f.store
        .complete_output_cleanup(&c, &receipt, true)
        .await
        .unwrap();
    expire_response(&f).await;
    let before = row(&f).await;
    let mut lock = pool.begin().await.unwrap();
    sqlx::query("SELECT operation_id FROM output_cleanup WHERE operation_id=$1 FOR UPDATE")
        .bind(f.operation.uuid())
        .fetch_one(&mut *lock)
        .await
        .unwrap();
    let store = f.store.clone();
    let task = tokio::spawn(async move { store.compact_expired_response().await });
    tokio::time::timeout(Duration::from_secs(5),async {
        loop {
            let waiting:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE datname=current_database() AND wait_event_type='Lock' AND query LIKE 'SELECT manifest,receipt,eligible_at%')")
                .fetch_one(&pool).await.unwrap();
            if waiting {break;}
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }).await.unwrap();
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    lock.commit().await.unwrap();
    assert_eq!(row(&f).await, before);
    assert_eq!(
        f.store.compact_expired_response().await.unwrap(),
        Compaction::Completed(f.operation)
    );
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
#[ignore = "requires PostgreSQL and private versioned MinIO fixture"]
async fn output_minio_compaction_preserves_retirement_receipts_after_payload_removal(pool: PgPool) {
    let (f, plans, refs) = archive_fixture(&pool, true).await;
    let refs = refs.unwrap();
    expire_response(&f).await;
    let worker = Cleaner::new(f.store.clone(), s3_config().build_retirer().unwrap(), true)
        .with_payload_compaction();
    assert_eq!(
        worker.tick().await.unwrap(),
        CleanupTick::Completed(f.operation)
    );
    let receipt = stored(&f).await.unwrap();
    assert_eq!(
        worker.tick().await.unwrap(),
        CleanupTick::PayloadCompaction(Compaction::Completed(f.operation))
    );
    assert_eq!(stored(&f).await.unwrap(), receipt);
    assert_eq!(row(&f).await["payload"], json!({}));
    let (revision,): (i64,) =
        sqlx::query_as("SELECT claim_revision FROM output_cleanup WHERE operation_id=$1")
            .bind(f.operation.uuid())
            .fetch_one(&pool)
            .await
            .unwrap();
    let claim = CleanupClaim {
        operation_id: f.operation,
        revision,
        lease_expires_at: OffsetDateTime::now_utc(),
    };
    f.store
        .complete_output_cleanup(&claim, &receipt, true)
        .await
        .unwrap();
    let storage = s3_config().build().unwrap();
    for reference in [&refs.stdout, &refs.stderr] {
        assert!(matches!(
            storage
                .read(
                    reference,
                    &plans.stdout.owner,
                    reference.plan.created_unix_ms + 1,
                    0,
                    1
                )
                .await,
            Err(sandbox_artifacts::Error::Missing)
        ));
    }
    assert_eq!(
        f.store
            .output_for_project(f.project, f.operation)
            .await
            .unwrap()
            .unwrap()
            .status,
        "expired"
    );
    assert_eq!(worker.tick().await.unwrap(), CleanupTick::Idle);
}
