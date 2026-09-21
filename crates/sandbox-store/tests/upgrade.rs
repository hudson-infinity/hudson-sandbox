//! Apply the second migration over real first-version data without inventing observations.
#![allow(clippy::unwrap_used)]
use sandbox_store::MIGRATOR;
use sqlx::{PgPool, migrate::Migrator};
use std::borrow::Cow;

#[sqlx::test(migrations = false)]
async fn observation_source_upgrade_preserves_existing_rows(pool: PgPool) {
    let initial = Migrator {
        migrations: Cow::Owned(vec![MIGRATOR.iter().next().unwrap().clone()]),
        ..Migrator::DEFAULT
    };
    initial.run(&pool).await.unwrap();
    let project = uuid::Uuid::now_v7();
    let sandbox = uuid::Uuid::now_v7();
    sqlx::query("INSERT INTO projects(id,name,status,limits) VALUES($1,'upgrade','active','{}')")
        .bind(project)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO sandboxes(id,project_id,image_digest,resources,desired_state,observed_state) VALUES($1,$2,'sha256:old','{}','running','creating')")
        .bind(sandbox).bind(project).execute(&pool).await.unwrap();
    MIGRATOR.run(&pool).await.unwrap();
    MIGRATOR.run(&pool).await.unwrap();
    let (state, source): (String, Option<bool>) =
        sqlx::query_as("SELECT observed_state,observation_simulated FROM sandboxes WHERE id=$1")
            .bind(sandbox)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(state, "creating");
    assert_eq!(source, None);
}

#[sqlx::test(migrations = false)]
async fn maintenance_upgrade_preserves_existing_execution_lease(pool: PgPool) {
    let previous = Migrator {
        migrations: Cow::Owned(MIGRATOR.iter().take(2).cloned().collect()),
        ..Migrator::DEFAULT
    };
    previous.run(&pool).await.unwrap();
    let project = uuid::Uuid::now_v7();
    let host = uuid::Uuid::now_v7();
    let sandbox = uuid::Uuid::now_v7();
    let allocation = uuid::Uuid::now_v7();
    sqlx::query("INSERT INTO projects(id,name,status,limits) VALUES($1,'upgrade','active','{}')")
        .bind(project)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO hosts(id,status,cpu_capacity,memory_capacity_mib,disk_capacity_mib,supervisor_epoch) VALUES($1,'ready',4,8192,65536,1)").bind(host).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO sandboxes(id,project_id,image_digest,resources,desired_state,observed_state,generation) VALUES($1,$2,'sha256:old','{}','running','running',1)").bind(sandbox).bind(project).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO allocations(id,project_id,sandbox_id,host_id,generation,supervisor_epoch,vcpu,memory_mib,disk_mib,status,lease_expires_at) VALUES($1,$2,$3,$4,1,1,1,512,1024,'running',clock_timestamp()+interval '30 seconds')").bind(allocation).bind(project).bind(sandbox).bind(host).execute(&pool).await.unwrap();
    sqlx::query("UPDATE sandboxes SET current_allocation_id=$2 WHERE id=$1")
        .bind(sandbox)
        .bind(allocation)
        .execute(&pool)
        .await
        .unwrap();
    let (before,): (String,) = sqlx::query_as("SELECT lease_expires_at::text FROM allocations")
        .fetch_one(&pool)
        .await
        .unwrap();
    MIGRATOR.run(&pool).await.unwrap();
    MIGRATOR.run(&pool).await.unwrap();
    let (after,revision,pending,empty):(String,i64,bool,bool)=sqlx::query_as("SELECT lease_expires_at::text,maintenance_revision,renewal_pending,
        lease_requested_until IS NULL AND lease_observation IS NULL AND maintenance_next_at IS NULL AND maintenance_lease_until IS NULL FROM allocations").fetch_one(&pool).await.unwrap();
    assert_eq!(before, after);
    assert_eq!(revision, 0);
    assert!(!pending);
    assert!(empty);
}

#[sqlx::test(migrations = false)]
async fn execution_upgrade_preserves_legacy_rows_without_inventing_ownership(pool: PgPool) {
    let previous = Migrator {
        migrations: Cow::Owned(MIGRATOR.iter().take(4).cloned().collect()),
        ..Migrator::DEFAULT
    };
    previous.run(&pool).await.unwrap();
    let project = uuid::Uuid::now_v7();
    let sandbox = uuid::Uuid::now_v7();
    sqlx::query("INSERT INTO projects(id,name,status,limits) VALUES($1,'upgrade','active','{}')")
        .bind(project)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO sandboxes(id,project_id,image_digest,resources,desired_state,observed_state) VALUES($1,$2,'sha256:legacy','{}','running','unknown')")
        .bind(sandbox).bind(project).execute(&pool).await.unwrap();
    for _ in 0..2 {
        sqlx::query("INSERT INTO operations(id,project_id,sandbox_id,kind,initiator_kind,idempotency_key,request_digest,digest_version,payload,status) VALUES($1,$2,$3,'execute','service',$4,$5,1,'{}','unknown')")
            .bind(uuid::Uuid::now_v7()).bind(project).bind(sandbox).bind(uuid::Uuid::now_v7().to_string()).bind(vec![0u8;32]).execute(&pool).await.unwrap();
    }
    let (before,): (serde_json::Value,) =
        sqlx::query_as("SELECT jsonb_agg(to_jsonb(o) ORDER BY id) FROM operations o")
            .fetch_one(&pool)
            .await
            .unwrap();
    let target = Migrator {
        migrations: Cow::Owned(MIGRATOR.iter().take(5).cloned().collect()),
        ..Migrator::DEFAULT
    };
    target.run(&pool).await.unwrap();
    target.run(&pool).await.unwrap();
    let (after,): (serde_json::Value,) = sqlx::query_as(
        "SELECT jsonb_agg(to_jsonb(o)-'execution_allocation_id' ORDER BY id) FROM operations o",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(before, after);
    let (n,):(i64,)=sqlx::query_as("SELECT count(*) FROM operations WHERE execution_allocation_id IS NULL AND status='unknown' AND attempt_count=0")
        .fetch_one(&pool).await.unwrap();
    assert_eq!(n, 2);
    let store = sandbox_store::Store::from_pool(pool);
    let claim = store
        .claim_next(sandbox_store::claims::OperationKind::Execute, 30)
        .await
        .unwrap()
        .unwrap();
    use sandbox_protocol::Id;
    assert!(matches!(
        store
            .prepare_execute(&claim, sandbox_protocol::HostId::generate(), 1)
            .await,
        Err(sandbox_store::dispatch::DispatchError::InvalidData)
    ));
}
