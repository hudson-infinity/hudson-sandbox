//! Constraint tests against a real PostgreSQL.
//!
//! These exist because the database is where several of the lifecycle
//! contract's guarantees actually live. "At most one live allocation per
//! sandbox" is not an invariant the application can be trusted to maintain
//! under concurrency — it is a unique index, and these tests prove the index
//! is there and bites.
//!
//! Set `DATABASE_URL` to run them (`make up` provides one locally). Without
//! it they skip loudly rather than passing quietly.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use sandbox_store::Store;
use sqlx::{Executor, PgPool, Postgres, Transaction};
use uuid::Uuid;

/// Connect and migrate, or skip. Returns `None` when `DATABASE_URL` is unset.
async fn pool() -> Option<PgPool> {
    let Ok(url) = std::env::var("DATABASE_URL") else {
        eprintln!("skipping: DATABASE_URL is unset — run `make up` and export it");
        return None;
    };

    let store = Store::connect(&url, 5).await.expect("connect");
    store.migrate().await.expect("migrate");
    Some(store.pool().clone())
}

/// Every test runs in a transaction that is never committed, so they share one
/// database without sharing state.
macro_rules! with_tx {
    (|$tx:ident| $body:block) => {{
        let Some(pool) = pool().await else { return };
        let mut $tx = pool.begin().await.expect("begin");
        $body
        // Dropped without commit: rolled back.
    }};
}

/// Insert a project and return its id.
async fn project(tx: &mut Transaction<'_, Postgres>) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO projects (id, name, status, limits) VALUES ($1, 'test', 'active', '{}')",
    )
    .bind(id)
    .execute(&mut **tx)
    .await
    .expect("insert project");
    id
}

/// Insert a host with generous capacity and return its id.
async fn host(tx: &mut Transaction<'_, Postgres>) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO hosts (id, status, cpu_capacity, memory_capacity_mib, disk_capacity_mib)
         VALUES ($1, 'ready', 32, 131072, 1048576)",
    )
    .bind(id)
    .execute(&mut **tx)
    .await
    .expect("insert host");
    id
}

/// Insert a running sandbox owned by `project_id`.
async fn sandbox(tx: &mut Transaction<'_, Postgres>, project_id: Uuid) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO sandboxes
             (id, project_id, image_digest, resources, desired_state, observed_state)
         VALUES ($1, $2, 'sha256:0000', '{\"vcpu\":2}', 'running', 'running')",
    )
    .bind(id)
    .bind(project_id)
    .execute(&mut **tx)
    .await
    .expect("insert sandbox");
    id
}

/// Insert an allocation, returning the database's answer rather than panicking,
/// so a test can assert on rejection.
async fn insert_allocation(
    tx: &mut Transaction<'_, Postgres>,
    project_id: Uuid,
    sandbox_id: Uuid,
    host_id: Uuid,
    generation: i64,
    released: bool,
) -> Result<(), sqlx::Error> {
    let released_at = if released { "now()" } else { "NULL" };
    let evidence = if released {
        "'{\"stopped\":true}'"
    } else {
        "NULL"
    };
    let status = if released { "released" } else { "running" };

    tx.execute(
        sqlx::query(&format!(
            "INSERT INTO allocations
                 (id, project_id, sandbox_id, host_id, generation, supervisor_epoch,
                  vcpu, memory_mib, disk_mib, status, released_at, release_evidence)
             VALUES ($1, $2, $3, $4, $5, 1, 2, 2048, 8192, '{status}', {released_at}, {evidence})"
        ))
        .bind(Uuid::now_v7())
        .bind(project_id)
        .bind(sandbox_id)
        .bind(host_id)
        .bind(generation),
    )
    .await
    .map(|_| ())
}

/// Insert an operation, returning the database's answer.
async fn insert_operation(
    tx: &mut Transaction<'_, Postgres>,
    project_id: Uuid,
    sandbox_id: Uuid,
    idempotency_key: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO operations
             (id, project_id, sandbox_id, kind, initiator_kind, idempotency_key,
              request_digest, digest_version, payload, status)
         VALUES ($1, $2, $3, 'execute', 'project', $4, '\\x00', 1, '{}', 'queued')",
    )
    .bind(Uuid::now_v7())
    .bind(project_id)
    .bind(sandbox_id)
    .bind(idempotency_key)
    .execute(&mut **tx)
    .await
    .map(|_| ())
}

#[tokio::test]
async fn migrations_apply() {
    let Some(pool) = pool().await else { return };

    let tables: Vec<(String,)> = sqlx::query_as(
        "SELECT tablename FROM pg_tables WHERE schemaname = 'public' ORDER BY tablename",
    )
    .fetch_all(&pool)
    .await
    .expect("list tables");

    let names: Vec<&str> = tables.iter().map(|(t,)| t.as_str()).collect();
    for expected in [
        "allocations",
        "hosts",
        "operations",
        "projects",
        "sandboxes",
    ] {
        assert!(names.contains(&expected), "missing table {expected}");
    }
}

#[tokio::test]
async fn migrations_are_idempotent() {
    let Some(_) = pool().await else { return };
    // `pool()` migrates. Migrating a second time must be a no-op rather than
    // an error, because every binary runs migrations on start.
    let Some(_) = pool().await else { return };
}

#[tokio::test]
async fn a_sandbox_has_at_most_one_unreleased_allocation() {
    with_tx!(|tx| {
        let p = project(&mut tx).await;
        let h = host(&mut tx).await;
        let s = sandbox(&mut tx, p).await;

        insert_allocation(&mut tx, p, s, h, 1, false)
            .await
            .expect("first live allocation");

        let second = insert_allocation(&mut tx, p, s, h, 2, false).await;
        assert!(
            second.is_err(),
            "two live allocations for one sandbox were accepted"
        );
    });
}

#[tokio::test]
async fn a_released_allocation_frees_the_slot() {
    with_tx!(|tx| {
        let p = project(&mut tx).await;
        let h = host(&mut tx).await;
        let s = sandbox(&mut tx, p).await;

        insert_allocation(&mut tx, p, s, h, 1, true)
            .await
            .expect("released allocation");
        insert_allocation(&mut tx, p, s, h, 2, false)
            .await
            .expect("replacement after release");
    });
}

#[tokio::test]
async fn a_generation_is_never_reused() {
    with_tx!(|tx| {
        let p = project(&mut tx).await;
        let h = host(&mut tx).await;
        let s = sandbox(&mut tx, p).await;

        insert_allocation(&mut tx, p, s, h, 1, true)
            .await
            .expect("first attempt, released");

        let reused = insert_allocation(&mut tx, p, s, h, 1, false).await;
        assert!(
            reused.is_err(),
            "a failed allocation's generation was reused"
        );
    });
}

#[tokio::test]
async fn a_release_requires_evidence() {
    with_tx!(|tx| {
        let p = project(&mut tx).await;
        let h = host(&mut tx).await;
        let s = sandbox(&mut tx, p).await;

        let claimed = sqlx::query(
            "INSERT INTO allocations
                 (id, project_id, sandbox_id, host_id, generation, supervisor_epoch,
                  vcpu, memory_mib, disk_mib, status, released_at)
             VALUES ($1, $2, $3, $4, 1, 1, 2, 2048, 8192, 'released', now())",
        )
        .bind(Uuid::now_v7())
        .bind(p)
        .bind(s)
        .bind(h)
        .execute(&mut *tx)
        .await;

        assert!(
            claimed.is_err(),
            "an allocation was marked released with no evidence of termination"
        );
    });
}

#[tokio::test]
async fn one_idempotency_key_per_project() {
    with_tx!(|tx| {
        let p = project(&mut tx).await;
        let s = sandbox(&mut tx, p).await;

        insert_operation(&mut tx, p, s, "key-that-is-long-enough")
            .await
            .expect("first admission");

        let duplicate = insert_operation(&mut tx, p, s, "key-that-is-long-enough").await;
        assert!(
            duplicate.is_err(),
            "the same key admitted two operations in one project"
        );
    });
}

#[tokio::test]
async fn the_same_key_may_be_used_by_a_different_project() {
    with_tx!(|tx| {
        let a = project(&mut tx).await;
        let b = project(&mut tx).await;
        let sandbox_a = sandbox(&mut tx, a).await;
        let sandbox_b = sandbox(&mut tx, b).await;

        insert_operation(&mut tx, a, sandbox_a, "shared-key-value-here")
            .await
            .expect("project a");
        insert_operation(&mut tx, b, sandbox_b, "shared-key-value-here")
            .await
            .expect("project b");
    });
}

#[tokio::test]
async fn an_operation_cannot_target_another_projects_sandbox() {
    with_tx!(|tx| {
        let a = project(&mut tx).await;
        let b = project(&mut tx).await;
        let victim = sandbox(&mut tx, b).await;

        let crossed = insert_operation(&mut tx, a, victim, "cross-tenant-key-1234").await;
        assert!(
            crossed.is_err(),
            "an operation in project a was accepted against project b's sandbox"
        );
    });
}

#[tokio::test]
async fn a_short_idempotency_key_is_rejected() {
    with_tx!(|tx| {
        let p = project(&mut tx).await;
        let s = sandbox(&mut tx, p).await;

        let short = insert_operation(&mut tx, p, s, "too-short").await;
        assert!(
            short.is_err(),
            "a key below the documented minimum was accepted"
        );
    });
}

#[tokio::test]
async fn only_a_cancel_carries_a_target() {
    with_tx!(|tx| {
        let p = project(&mut tx).await;
        let s = sandbox(&mut tx, p).await;

        let execute_with_target = sqlx::query(
            "INSERT INTO operations
                 (id, project_id, sandbox_id, kind, initiator_kind, idempotency_key,
                  request_digest, digest_version, payload, status, target_operation_id)
             VALUES ($1, $2, $3, 'execute', 'project', 'execute-with-target-1', '\\x00', 1, '{}',
                     'queued', $4)",
        )
        .bind(Uuid::now_v7())
        .bind(p)
        .bind(s)
        .bind(Uuid::now_v7())
        .execute(&mut *tx)
        .await;

        assert!(
            execute_with_target.is_err(),
            "a non-cancel operation carried a cancellation target"
        );
    });
}

#[tokio::test]
async fn a_terminal_operation_records_when_it_finished() {
    with_tx!(|tx| {
        let p = project(&mut tx).await;
        let s = sandbox(&mut tx, p).await;

        let succeeded_without_time = sqlx::query(
            "INSERT INTO operations
                 (id, project_id, sandbox_id, kind, initiator_kind, idempotency_key,
                  request_digest, digest_version, payload, status)
             VALUES ($1, $2, $3, 'execute', 'project', 'terminal-no-time-key', '\\x00', 1, '{}',
                     'succeeded')",
        )
        .bind(Uuid::now_v7())
        .bind(p)
        .bind(s)
        .execute(&mut *tx)
        .await;

        assert!(
            succeeded_without_time.is_err(),
            "an operation succeeded without recording when"
        );
    });
}
