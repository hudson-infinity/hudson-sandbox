use super::*;
use sandbox_protocol::{AllocationId, allocation_authority::Authority};

async fn counts(pool: &PgPool, host: HostId) -> (i64, i64, i64) {
    sqlx::query_as("SELECT last_allocation_serial,(SELECT count(*) FROM allocation_permits WHERE host_id=$1),(SELECT count(*) FROM allocations WHERE host_id=$1) FROM hosts WHERE id=$1")
        .bind(host.uuid()).fetch_one(pool).await.unwrap()
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn allocation_serial_retry_uses_original_identity_and_epoch(pool: PgPool) {
    let (store, claim, host) = fixture(&pool).await;
    store.reserve_create(&claim, host, 1).await.unwrap();
    let first = store.allocation_permits_after(host, 0).await.unwrap();
    assert!(!first.has_unissued_allocations);
    assert_eq!(first.issued_through, 1);
    assert_eq!(first.permits.len(), 1);
    assert_eq!(first.permits[0].create_operation, claim.operation_id);
    assert_eq!(first.permits[0].sandbox, claim.sandbox_id);
    sqlx::query("UPDATE hosts SET supervisor_epoch=2 WHERE id=$1")
        .bind(host.uuid())
        .execute(&pool)
        .await
        .unwrap();
    assert!(matches!(
        store.reserve_create(&claim, host, 2).await.unwrap(),
        Reservation::Existing(_)
    ));
    let retry = store.allocation_permits_after(host, 0).await.unwrap();
    assert_eq!(retry.permits, first.permits);
    assert_eq!(retry.permits[0].original_epoch, 1);
    assert_eq!(counts(&pool, host).await, (1, 1, 1));
    let mut authority = Authority::new(host).unwrap();
    authority.register(&retry.permits).unwrap();
    authority.authorize(&retry.permits[0]).unwrap();
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn allocation_serial_concurrent_reservations_are_contiguous(pool: PgPool) {
    let host = host(&pool).await;
    sqlx::query("UPDATE hosts SET cpu_capacity=32,memory_capacity_mib=32768,disk_capacity_mib=131072 WHERE id=$1")
        .bind(host.uuid()).execute(&pool).await.unwrap();
    let store = Store::from_pool(pool.clone());
    let mut claims = Vec::new();
    for _ in 0..8 {
        let (project, token) = project(&pool).await;
        claims.push(claim(&store, project, &token, 30).await);
    }
    let mut tasks = tokio::task::JoinSet::new();
    for claim in claims {
        let store = store.clone();
        tasks.spawn(async move { store.reserve_create(&claim, host, 1).await });
    }
    while let Some(result) = tasks.join_next().await {
        assert!(matches!(result.unwrap().unwrap(), Reservation::Reserved(_)));
    }
    let batch = store.allocation_permits_after(host, 0).await.unwrap();
    assert_eq!(
        batch.permits.iter().map(|p| p.serial).collect::<Vec<_>>(),
        (1..=8).collect::<Vec<_>>()
    );
    assert_eq!(counts(&pool, host).await, (8, 8, 8));
    let mut authority = Authority::new(host).unwrap();
    authority.register(&batch.permits).unwrap();
    for permit in batch.permits.iter().rev() {
        authority.authorize(permit).unwrap();
    }
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn allocation_serial_failed_commit_does_not_consume_identity(pool: PgPool) {
    let (store, claim, host) = fixture(&pool).await;
    // Fail after allocation and serial issuance, at the final operation update.
    sqlx::raw_sql("CREATE FUNCTION reject_reserved() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.phase='reserved' THEN RAISE EXCEPTION 'injected final write failure'; END IF; RETURN NEW; END $$; CREATE TRIGGER reject_reserved BEFORE UPDATE ON operations FOR EACH ROW EXECUTE FUNCTION reject_reserved();")
        .execute(&pool).await.unwrap();
    assert!(store.reserve_create(&claim, host, 1).await.is_err());
    assert_eq!(counts(&pool, host).await, (0, 0, 0));
    sqlx::query("DROP TRIGGER reject_reserved ON operations")
        .execute(&pool)
        .await
        .unwrap();
    store.reserve_create(&claim, host, 1).await.unwrap();
    assert_eq!(counts(&pool, host).await, (1, 1, 1));
    assert_eq!(
        store
            .allocation_permits_after(host, 0)
            .await
            .unwrap()
            .permits[0]
            .serial,
        1
    );
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn allocation_serial_exhaustion_rolls_back_reservation(pool: PgPool) {
    let (store, claim, host) = fixture(&pool).await;
    sqlx::query("UPDATE hosts SET last_allocation_serial=9223372036854775807 WHERE id=$1")
        .bind(host.uuid())
        .execute(&pool)
        .await
        .unwrap();
    assert!(matches!(
        store.reserve_create(&claim, host, 1).await,
        Err(PlacementError::Capacity)
    ));
    assert_eq!(counts(&pool, host).await, (i64::MAX, 0, 0));
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn allocation_serial_reads_reject_gaps_and_changed_owner(pool: PgPool) {
    let (store, claim, host) = fixture(&pool).await;
    store.reserve_create(&claim, host, 1).await.unwrap();
    assert!(store.allocation_permits_after(host, 2).await.is_err());
    assert!(
        store
            .allocation_permits_after(host, u64::MAX)
            .await
            .is_err()
    );
    sqlx::query("UPDATE allocation_permits SET original_epoch=2")
        .execute(&pool)
        .await
        .unwrap();
    assert!(store.allocation_permits_after(host, 0).await.is_err());
    sqlx::query("UPDATE allocation_permits SET original_epoch=1")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM allocation_permits")
        .execute(&pool)
        .await
        .unwrap();
    assert!(store.allocation_permits_after(host, 0).await.is_err());
    let after = store.allocation_permits_after(host, 1).await.unwrap();
    assert!(after.has_unissued_allocations);
    assert!(after.permits.is_empty());
}

#[sqlx::test(migrations = false)]
async fn allocation_serial_upgrade_preserves_legacy_owner_without_inventing_permit(pool: PgPool) {
    let old = sqlx::migrate::Migrator {
        migrations: std::borrow::Cow::Owned(
            sandbox_store::MIGRATOR.iter().take(16).cloned().collect(),
        ),
        ..sqlx::migrate::Migrator::DEFAULT
    };
    old.run(&pool).await.unwrap();
    let (store, claim, host) = fixture(&pool).await;
    let allocation = AllocationId::generate();
    sqlx::query("INSERT INTO allocations(id,project_id,sandbox_id,host_id,generation,supervisor_epoch,vcpu,memory_mib,disk_mib,status) VALUES($1,$2,$3,$4,1,1,2,2048,8192,'reserved')")
        .bind(allocation.uuid()).bind(claim.project_id.uuid()).bind(claim.sandbox_id.uuid()).bind(host.uuid()).execute(&pool).await.unwrap();
    sqlx::query("UPDATE sandboxes SET current_allocation_id=$1,generation=1 WHERE id=$2")
        .bind(allocation.uuid())
        .bind(claim.sandbox_id.uuid())
        .execute(&pool)
        .await
        .unwrap();
    let before: serde_json::Value = sqlx::query_scalar("SELECT to_jsonb(a) FROM allocations a")
        .fetch_one(&pool)
        .await
        .unwrap();
    sandbox_store::MIGRATOR.run(&pool).await.unwrap();
    let new_pool = sqlx::postgres::PgPoolOptions::new()
        .connect_with(pool.connect_options().as_ref().clone())
        .await
        .unwrap();
    drop(store);
    let store = Store::from_pool(new_pool.clone());
    let after: serde_json::Value = sqlx::query_scalar("SELECT to_jsonb(a) FROM allocations a")
        .fetch_one(&new_pool)
        .await
        .unwrap();
    assert_eq!(before, after);
    let batch = store.allocation_permits_after(host, 0).await.unwrap();
    assert!(batch.has_unissued_allocations);
    assert!(batch.permits.is_empty());
    assert_eq!(batch.issued_through, 0);
    assert!(matches!(
        store.reserve_create(&claim, host, 1).await.unwrap(),
        Reservation::Existing(_)
    ));
    assert_eq!(counts(&new_pool, host).await, (0, 0, 1));
    new_pool.close().await;
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn allocation_serial_batches_are_bounded_and_host_scoped(pool: PgPool) {
    let host = host(&pool).await;
    sqlx::query("UPDATE hosts SET cpu_capacity=128,memory_capacity_mib=262144,disk_capacity_mib=1048576 WHERE id=$1")
        .bind(host.uuid()).execute(&pool).await.unwrap();
    let store = Store::from_pool(pool.clone());
    for _ in 0..35 {
        let (project, token) = project(&pool).await;
        let claim = claim(&store, project, &token, 30).await;
        store.reserve_create(&claim, host, 1).await.unwrap();
    }
    let first = store.allocation_permits_after(host, 0).await.unwrap();
    let second = store.allocation_permits_after(host, 32).await.unwrap();
    assert_eq!(
        (
            first.issued_through,
            first.permits.len(),
            second.permits.len()
        ),
        (35, 32, 3)
    );
    let mut authority = Authority::new(host).unwrap();
    authority.register(&first.permits).unwrap();
    authority.register(&second.permits).unwrap();
    assert_eq!(authority.through(), 35);
    assert!(
        store
            .allocation_permits_after(host, 35)
            .await
            .unwrap()
            .permits
            .is_empty()
    );
    let other = super::host(&pool).await;
    let other_batch = store.allocation_permits_after(other, 0).await.unwrap();
    assert_eq!(other_batch.issued_through, 0);
    assert!(other_batch.permits.is_empty());
    assert!(!other_batch.has_unissued_allocations);
}
