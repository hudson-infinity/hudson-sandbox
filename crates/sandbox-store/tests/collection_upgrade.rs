//! Collection indexes upgrade populated tables without changing resource history.
#![allow(clippy::unwrap_used)]
use sandbox_store::MIGRATOR;
use sqlx::{PgPool, migrate::Migrator};
use std::borrow::Cow;

#[sqlx::test(migrations = false)]
async fn collection_index_upgrade_preserves_rows_and_installs_scoped_sort_indexes(pool: PgPool) {
    let previous = Migrator {
        migrations: Cow::Owned(MIGRATOR.iter().filter(|m| m.version < 4).cloned().collect()),
        ..Migrator::DEFAULT
    };
    previous.run(&pool).await.unwrap();
    let project = uuid::Uuid::now_v7();
    let sandbox = uuid::Uuid::now_v7();
    let operation = uuid::Uuid::now_v7();
    sqlx::query(
        "INSERT INTO projects(id,name,status,limits) VALUES($1,'index-upgrade','active','{}')",
    )
    .bind(project)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO sandboxes(id,project_id,image_digest,resources,desired_state,observed_state,created_at)
        VALUES($1,$2,'sha256:existing','{}','running','unknown','2026-01-01T00:00:00.123456Z')").bind(sandbox).bind(project).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO operations(id,project_id,sandbox_id,kind,initiator_kind,idempotency_key,request_digest,digest_version,payload,status,created_at)
        VALUES($1,$2,$3,'create','service',$4,$5,1,'{}','unknown','2026-01-01T00:00:00.123456Z')")
        .bind(operation).bind(project).bind(sandbox).bind(operation.to_string()).bind(vec![0u8;32]).execute(&pool).await.unwrap();
    let (sandbox_before, operation_before): (serde_json::Value, serde_json::Value) =
        sqlx::query_as(
            "SELECT (SELECT to_jsonb(s) FROM sandboxes s),(SELECT to_jsonb(o) FROM operations o)",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
    MIGRATOR.run(&pool).await.unwrap();
    MIGRATOR.run(&pool).await.unwrap();
    let (sandbox_after, operation_after): (serde_json::Value, serde_json::Value) = sqlx::query_as(
        "SELECT (SELECT to_jsonb(s) FROM sandboxes s),(SELECT to_jsonb(o) FROM operations o)",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(sandbox_before, sandbox_after);
    assert_eq!(operation_before, operation_after);
    let definitions:Vec<(String,String)>=sqlx::query_as("SELECT indexname,indexdef FROM pg_indexes WHERE schemaname=current_schema()
        AND indexname IN ('sandboxes_project_created_idx','operations_project_created_idx','operations_project_sandbox_created_idx')").fetch_all(&pool).await.unwrap();
    assert_eq!(definitions.len(), 3);
    for (name, definition) in definitions {
        let key = if name == "operations_project_sandbox_created_idx" {
            "(project_id, sandbox_id, created_at DESC, id DESC)"
        } else {
            "(project_id, created_at DESC, id DESC)"
        };
        assert!(definition.contains(key), "{definition}");
    }
}
