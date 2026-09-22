//! Scheduling tests do not treat a candidate as a physical cleanup proof.
use super::*;

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn scan_requires_current_registered_physical_host_and_retries_durably(pool: PgPool) {
    let f = Fixture::new(&pool).await;
    assert!(f.store.next_allocation_retirement(f.host, 1).await.unwrap().is_none());
    released(&f).await;
    assert!(f.store.next_allocation_retirement(f.host, 2).await.unwrap().is_none());
    sqlx::query("UPDATE hosts SET registered_allocation_serial=0 WHERE id=$1")
        .bind(f.host.uuid()).execute(&pool).await.unwrap();
    assert!(f.store.next_allocation_retirement(f.host, 1).await.unwrap().is_none());
    sqlx::query("UPDATE hosts SET registered_allocation_serial=1 WHERE id=$1")
        .bind(f.host.uuid()).execute(&pool).await.unwrap();
    let selected = f.store.next_allocation_retirement(f.host, 1).await.unwrap().unwrap();
    assert_eq!(selected.allocation, f.allocation);
    assert!(selected.intent.is_none());
    let restarted = Store::from_pool(pool.clone());
    assert!(restarted.next_allocation_retirement(f.host, 1).await.unwrap().is_none());
    sqlx::query("UPDATE allocations SET retirement_scan_at=clock_timestamp()-interval '6 seconds'")
        .execute(&pool).await.unwrap();
    assert_eq!(restarted.next_allocation_retirement(f.host, 1).await.unwrap().unwrap().allocation, f.allocation);
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn concurrent_scanners_select_one_owner_once_and_skip_delivery_leases(pool: PgPool) {
    let f = Fixture::new(&pool).await;
    released(&f).await;
    let mut tasks = tokio::task::JoinSet::new();
    for _ in 0..8 {
        let store = f.store.clone();
        let host = f.host;
        tasks.spawn(async move { store.next_allocation_retirement(host, 1).await.unwrap() });
    }
    let mut selected = 0;
    while let Some(result) = tasks.join_next().await {
        selected += usize::from(result.unwrap().is_some());
    }
    assert_eq!(selected, 1);
    let request = prepare(&f).await.unwrap().unwrap();
    sqlx::query("UPDATE allocations SET retirement_scan_at=NULL").execute(&pool).await.unwrap();
    assert!(f.store.next_allocation_retirement(f.host, 1).await.unwrap().is_none());
    expire(&f).await;
    let selected = f.store.next_allocation_retirement(f.host, 1).await.unwrap().unwrap();
    assert_eq!(selected.intent, Some(request.intent));
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn unfinished_candidates_do_not_starve_later_allocations(pool: PgPool) {
    let first = Fixture::new(&pool).await;
    released(&first).await;
    let mut expected = std::collections::BTreeSet::from([first.allocation]);
    // Synthetic scheduling-only owners share a host. Full preparation still
    // independently checks their release receipts; no RPC uses these hints.
    for serial in 2..=34_i64 {
        let f = Fixture::new(&pool).await;
        released(&f).await;
        sqlx::query("UPDATE allocations SET host_id=$2 WHERE id=$1")
            .bind(f.allocation.uuid()).bind(first.host.uuid()).execute(&pool).await.unwrap();
        sqlx::query("UPDATE allocation_permits SET host_id=$2,serial=$3 WHERE allocation_id=$1")
            .bind(f.allocation.uuid()).bind(first.host.uuid()).bind(serial).execute(&pool).await.unwrap();
        expected.insert(f.allocation);
    }
    sqlx::query("UPDATE hosts SET last_allocation_serial=34,registered_allocation_serial=34 WHERE id=$1")
        .bind(first.host.uuid()).execute(&pool).await.unwrap();
    for _ in 0..34 {
        let candidate = first.store.next_allocation_retirement(first.host, 1).await.unwrap().unwrap();
        assert!(expected.remove(&candidate.allocation), "candidate was repeated before all owners received a turn");
    }
    assert!(expected.is_empty());
}
