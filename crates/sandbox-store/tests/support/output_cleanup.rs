use super::*;
use sandbox_store::output_cleanup::{CleanupClaim, CleanupManifest, CleanupPreparation};

#[path = "cleanup_completion.rs"]
mod completion_tests;

// Move this fixture's entire saved retention policy together, preserving exact
// equality between ticket, plans and references. No shared database is reset.
async fn age(f: &Fixture, future_grace: bool) {
    let row =
        sqlx::query("SELECT output_ticket,output_plan,output_refs FROM operations WHERE id=$1")
            .bind(f.operation.uuid())
            .fetch_one(f.store.pool())
            .await
            .unwrap();
    let mut ticket: sandbox_protocol::output::OutputTicket =
        serde_json::from_value(row.get("output_ticket")).unwrap();
    ticket.created_unix_ms = now() - 4000;
    ticket.expires_unix_ms = ticket.created_unix_ms + 1000;
    ticket.delete_after_unix_ms = if future_grace {
        now() + 60_000
    } else {
        now() - 1000
    };
    let adjust = |p: &mut OutputPlan| {
        p.created_unix_ms = ticket.created_unix_ms;
        p.expires_unix_ms = ticket.expires_unix_ms;
        p.delete_after_unix_ms = ticket.delete_after_unix_ms;
    };
    let mut p: Option<OutputPlans> = row
        .get::<Option<Value>, _>("output_plan")
        .map(|p| serde_json::from_value(p).unwrap());
    if let Some(p) = &mut p {
        adjust(&mut p.stdout);
        adjust(&mut p.stderr);
    }
    let mut r: Vec<OutputRef> = serde_json::from_value(row.get("output_refs")).unwrap();
    for r in &mut r {
        adjust(&mut r.plan);
    }
    sqlx::query(
        "UPDATE operations SET output_ticket=$2,output_plan=$3,output_refs=$4,
        output_expires_at=$5,completed_at=$6 WHERE id=$1",
    )
    .bind(f.operation.uuid())
    .bind(json!(ticket))
    .bind(p.map(|p| json!(p)))
    .bind(json!(r))
    .bind(
        OffsetDateTime::from_unix_timestamp_nanos(i128::from(ticket.expires_unix_ms) * 1_000_000)
            .unwrap(),
    )
    .bind(
        OffsetDateTime::from_unix_timestamp_nanos(i128::from(ticket.created_unix_ms) * 1_000_000)
            .unwrap(),
    )
    .execute(f.store.pool())
    .await
    .unwrap();
}
async fn published(pool: &PgPool) -> (Fixture, OutputClaim) {
    let f = Fixture::new(pool).await;
    f.finish().await;
    let (claim, work) = f.work().await;
    let p = plans(&work);
    f.store.save_output_plans(&claim, &p, true).await.unwrap();
    f.store
        .publish_output(&claim, &refs(&p), true)
        .await
        .unwrap();
    (f, claim)
}
async fn next(f: &Fixture) -> CleanupClaim {
    f.store.claim_output_cleanup(30).await.unwrap().unwrap()
}
async fn ready(f: &Fixture, c: &CleanupClaim) -> CleanupManifest {
    let CleanupPreparation::Ready { manifest } = f.store.prepare_output_cleanup(c).await.unwrap()
    else {
        panic!("ready")
    };
    *manifest
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn cleanup_concurrent_inventory_is_exact_and_recoverable(pool: PgPool) {
    let (f, publisher) = published(&pool).await;
    assert_eq!(f.store.enqueue_expired_output(100).await.unwrap(), 0);
    age(&f, false).await;
    let before = f.snapshot().await;
    let (a, b) = tokio::join!(
        f.store.enqueue_expired_output(100),
        f.store.enqueue_expired_output(100)
    );
    assert_eq!(a.unwrap() + b.unwrap(), 1);
    let (a, b) = tokio::join!(
        f.store.claim_output_cleanup(30),
        f.store.claim_output_cleanup(30)
    );
    let claims: Vec<_> = [a.unwrap(), b.unwrap()].into_iter().flatten().collect();
    assert_eq!(claims.len(), 1);
    let c = &claims[0];
    let manifest = ready(&f, c).await;
    assert_eq!(manifest.ticket.owner.operation_id, f.operation);
    assert!(manifest.simulated);
    assert_eq!(
        manifest.references.as_ref().unwrap().plans(),
        manifest.plans.clone().unwrap()
    );
    assert!(matches!(
        f.store
            .publish_output(&publisher, manifest.references.as_ref().unwrap(), true)
            .await,
        Err(OutputError::LostClaim)
    ));
    assert!(f.store.claim_output(30).await.unwrap().is_none());
    assert_eq!(f.snapshot().await, before);
    assert_eq!(ready(&f, c).await, manifest);
    sqlx::query("UPDATE output_cleanup SET lease_expires_at=clock_timestamp()-interval '1 second'")
        .execute(&pool)
        .await
        .unwrap();
    let replacement = next(&f).await;
    assert!(replacement.revision > c.revision);
    assert!(matches!(
        f.store.prepare_output_cleanup(c).await,
        Err(OutputError::LostClaim)
    ));
    assert!(matches!(
        f.store.defer_output_cleanup(c, 1).await,
        Err(OutputError::LostClaim)
    ));
    assert_eq!(ready(&f, &replacement).await, manifest);
    assert_eq!(f.snapshot().await, before);
    assert_eq!(f.store.enqueue_expired_output(100).await.unwrap(), 0);
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn cleanup_keeps_orphan_plans_and_respects_grace(pool: PgPool) {
    let f = Fixture::new(&pool).await;
    f.finish().await;
    let (publisher, work) = f.work().await;
    let p = plans(&work);
    f.store
        .save_output_plans(&publisher, &p, true)
        .await
        .unwrap();
    age(&f, true).await;
    let before = f.snapshot().await;
    assert_eq!(f.store.enqueue_expired_output(10).await.unwrap(), 1);
    let c = next(&f).await;
    let CleanupPreparation::Waiting { eligible_at } =
        f.store.prepare_output_cleanup(&c).await.unwrap()
    else {
        panic!("grace")
    };
    assert!(eligible_at > OffsetDateTime::now_utc());
    assert!(f.store.claim_output_cleanup(30).await.unwrap().is_none());
    assert!(matches!(
        f.store.save_output_plans(&publisher, &p, true).await,
        Err(OutputError::LostClaim)
    ));
    let saved: Value =
        sqlx::query_scalar("SELECT manifest FROM output_cleanup WHERE operation_id=$1")
            .bind(f.operation.uuid())
            .fetch_one(&pool)
            .await
            .unwrap();
    let manifest: CleanupManifest = serde_json::from_value(saved).unwrap();
    assert!(manifest.plans.is_some());
    assert!(manifest.references.is_none());
    assert_eq!(f.snapshot().await, before);
    // A second operation with no authorized upload is still inventoried. The
    // first row's grace period does not block it.
    let g = Fixture::new(&pool).await;
    g.finish().await;
    g.work().await;
    age(&g, false).await;
    assert_eq!(g.store.enqueue_expired_output(10).await.unwrap(), 1);
    let c = next(&g).await;
    assert_eq!(c.operation_id, g.operation);
    let m = ready(&g, &c).await;
    assert!(m.plans.is_none() && m.references.is_none());
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn cleanup_defers_corruption_without_partial_expiry_or_starvation(pool: PgPool) {
    let (f, _) = published(&pool).await;
    age(&f, false).await;
    f.store.enqueue_expired_output(1).await.unwrap();
    sqlx::query("UPDATE operations SET output_ticket=jsonb_set(output_ticket,'{owner,boot_id}','\"wrong-boot\"') WHERE id=$1")
        .bind(f.operation.uuid()).execute(&pool).await.unwrap();
    let c = next(&f).await;
    assert!(matches!(
        f.store.prepare_output_cleanup(&c).await,
        Err(OutputError::Corrupt)
    ));
    let (status,manifest): (String,Option<Value>) = sqlx::query_as("SELECT o.output_status,c.manifest FROM operations o JOIN output_cleanup c ON c.operation_id=o.id WHERE o.id=$1")
        .bind(f.operation.uuid()).fetch_one(&pool).await.unwrap();
    assert_eq!(status, "published");
    assert!(manifest.is_none());
    f.store.defer_output_cleanup(&c, 60).await.unwrap();
    let (g, _) = published(&pool).await;
    age(&g, false).await;
    g.store.enqueue_expired_output(100).await.unwrap();
    let c = next(&g).await;
    assert_eq!(c.operation_id, g.operation);
    ready(&g, &c).await;
    sqlx::query("UPDATE output_cleanup SET manifest=jsonb_set(manifest,'{version}','2') WHERE operation_id=$1")
        .bind(g.operation.uuid()).execute(&pool).await.unwrap();
    assert!(matches!(
        g.store.prepare_output_cleanup(&c).await,
        Err(OutputError::Corrupt)
    ));
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn cleanup_expired_claim_after_lock_wait_cannot_freeze_publication(pool: PgPool) {
    let (f, _) = published(&pool).await;
    age(&f, false).await;
    f.store.enqueue_expired_output(1).await.unwrap();
    let c = f.store.claim_output_cleanup(1).await.unwrap().unwrap();
    let mut lock = pool.begin().await.unwrap();
    sqlx::query("SELECT id FROM operations WHERE id=$1 FOR UPDATE")
        .bind(f.operation.uuid())
        .fetch_one(&mut *lock)
        .await
        .unwrap();
    let store = f.store.clone();
    let worker = tokio::spawn(async move { store.prepare_output_cleanup(&c).await });
    tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
    lock.commit().await.unwrap();
    assert!(matches!(worker.await.unwrap(), Err(OutputError::LostClaim)));
    let (status,manifest): (String,Option<Value>) = sqlx::query_as("SELECT o.output_status,c.manifest FROM operations o JOIN output_cleanup c ON c.operation_id=o.id WHERE o.id=$1")
        .bind(f.operation.uuid()).fetch_one(&pool).await.unwrap();
    assert_eq!(status, "published");
    assert!(manifest.is_none());
    ready(&f, &next(&f).await).await;
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn cleanup_uses_original_owner_after_destroy_epoch_and_project_deletion(pool: PgPool) {
    let (f, _) = published(&pool).await;
    age(&f, false).await;
    let before = f.snapshot().await;
    sqlx::query("UPDATE sandboxes SET current_allocation_id=NULL WHERE id=$1")
        .bind(f.sandbox.uuid())
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE hosts SET supervisor_epoch=supervisor_epoch+1 WHERE id=$1")
        .bind(f.host.uuid())
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE projects SET status='deleting' WHERE id=$1")
        .bind(f.project.uuid())
        .execute(&pool)
        .await
        .unwrap();
    f.store.enqueue_expired_output(100).await.unwrap();
    let m = ready(&f, &next(&f).await).await;
    assert_eq!(m.ticket.owner.host_epoch, 1);
    assert_eq!(m.ticket.owner.allocation_id, f.allocation);
    assert_eq!(f.snapshot().await, before);
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn cleanup_rechecks_retention_and_bounds_even_for_prequeued_rows(pool: PgPool) {
    let (f, _) = published(&pool).await;
    for limit in [0, 101, u32::MAX] {
        assert!(matches!(
            f.store.enqueue_expired_output(limit).await,
            Err(OutputError::InvalidPolicy)
        ));
    }
    for seconds in [0, 301, u32::MAX] {
        assert!(matches!(
            f.store.claim_output_cleanup(seconds).await,
            Err(OutputError::InvalidPolicy)
        ));
    }
    // Even a bad queue insertion cannot grant authority before retention ends.
    sqlx::query("INSERT INTO output_cleanup(operation_id) VALUES($1)")
        .bind(f.operation.uuid())
        .execute(&pool)
        .await
        .unwrap();
    let c = next(&f).await;
    assert!(matches!(
        f.store.prepare_output_cleanup(&c).await,
        Err(OutputError::BadEvidence)
    ));
    for seconds in [0, 3601] {
        assert!(matches!(
            f.store.defer_output_cleanup(&c, seconds).await,
            Err(OutputError::InvalidPolicy)
        ));
    }
    let (status, manifest): (String, Option<Value>) = sqlx::query_as("SELECT o.output_status,c.manifest FROM operations o JOIN output_cleanup c ON c.operation_id=o.id WHERE o.id=$1")
        .bind(f.operation.uuid()).fetch_one(&pool).await.unwrap();
    assert_eq!(status, "published");
    assert!(manifest.is_none());
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn cleanup_final_fence_rolls_back_expiry_after_inventory_lock_wait(pool: PgPool) {
    let (f, _) = published(&pool).await;
    age(&f, false).await;
    f.store.enqueue_expired_output(1).await.unwrap();
    let c = f.store.claim_output_cleanup(1).await.unwrap().unwrap();
    let mut lock = pool.begin().await.unwrap();
    sqlx::query("SELECT operation_id FROM output_cleanup WHERE operation_id=$1 FOR UPDATE")
        .bind(f.operation.uuid())
        .fetch_one(&mut *lock)
        .await
        .unwrap();
    let store = f.store.clone();
    let worker = tokio::spawn(async move { store.prepare_output_cleanup(&c).await });
    tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
    lock.commit().await.unwrap();
    assert!(matches!(worker.await.unwrap(), Err(OutputError::LostClaim)));
    let (status, manifest): (String, Option<Value>) = sqlx::query_as("SELECT o.output_status,c.manifest FROM operations o JOIN output_cleanup c ON c.operation_id=o.id WHERE o.id=$1")
        .bind(f.operation.uuid()).fetch_one(&pool).await.unwrap();
    assert_eq!(status, "published");
    assert!(manifest.is_none());
}

#[sqlx::test(migrations = false)]
async fn cleanup_migration_preserves_existing_publication_and_execution(pool: PgPool) {
    let old = sqlx::migrate::Migrator {
        migrations: std::borrow::Cow::Owned(
            sandbox_store::MIGRATOR.iter().take(6).cloned().collect(),
        ),
        ..sqlx::migrate::Migrator::DEFAULT
    };
    old.run(&pool).await.unwrap();
    let (f, _) = published(&pool).await;
    age(&f, false).await;
    let before: Value = sqlx::query_scalar("SELECT to_jsonb(o) FROM operations o WHERE id=$1")
        .bind(f.operation.uuid())
        .fetch_one(&pool)
        .await
        .unwrap();
    sandbox_store::MIGRATOR.run(&pool).await.unwrap();
    sandbox_store::MIGRATOR.run(&pool).await.unwrap();
    let mut after: Value = sqlx::query_scalar("SELECT to_jsonb(o) FROM operations o WHERE id=$1")
        .bind(f.operation.uuid())
        .fetch_one(&pool)
        .await
        .unwrap();
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
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM output_cleanup")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
    assert_eq!(f.store.enqueue_expired_output(100).await.unwrap(), 1);
    ready(&f, &next(&f).await).await;
}
