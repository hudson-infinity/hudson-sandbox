//! Admission and deduplication against a real PostgreSQL.
//!
//! The deduplication guarantee is the reason a client can retry a create
//! without risking two sandboxes, so it is tested against the database rather
//! than against a mock — the constraint doing the work lives there.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use sandbox_protocol::{Id, IdempotencyKey, ProjectId, ProjectToken, RequestDigest, TokenKeyId};
use sandbox_store::Store;
use sandbox_store::admission::{Admission, CreateSandbox, Resources};
use serde_json::json;

async fn store() -> Option<Store> {
    let Ok(url) = std::env::var("DATABASE_URL") else {
        eprintln!("skipping: DATABASE_URL is unset — run `make up` and export it");
        return None;
    };
    let store = Store::connect(&url, 10).await.expect("connect");
    store.migrate().await.expect("migrate");
    Some(store)
}

async fn project(store: &Store) -> ProjectId {
    let id = ProjectId::generate();
    sqlx::query(
        "INSERT INTO projects (id, name, status, limits) VALUES ($1, 'admission', 'active', '{}')",
    )
    .bind(id.uuid())
    .execute(store.pool())
    .await
    .expect("insert project");
    id
}

async fn cleanup(store: &Store, project_id: ProjectId) {
    // Children first: the schema's foreign keys are doing their job.
    for statement in [
        "UPDATE sandboxes SET active_transition_operation_id = NULL WHERE project_id = $1",
        "DELETE FROM operations WHERE project_id = $1",
        "DELETE FROM sandboxes WHERE project_id = $1",
        "DELETE FROM projects WHERE id = $1",
    ] {
        sqlx::query(statement)
            .bind(project_id.uuid())
            .execute(store.pool())
            .await
            .expect("cleanup");
    }
}

fn request(project_id: ProjectId, key: &str, payload: serde_json::Value) -> CreateSandbox {
    let token = ProjectToken::generate().expect("generate");
    CreateSandbox {
        project_id,
        key_id: TokenKeyId::from_stored(token.key_id().as_str().to_owned()),
        idempotency_key: IdempotencyKey::parse(key).expect("valid key"),
        request_digest: RequestDigest::compute("POST", "/v1/sandboxes", &payload).expect("digest"),
        image_digest: format!("sha256:{}", "a".repeat(64)),
        name: Some("admission-test".to_owned()),
        resources: Resources {
            vcpu: 2,
            memory_mib: 2048,
            disk_mib: 8192,
        },
        payload,
    }
}

#[tokio::test]
async fn admits_a_new_request() {
    let Some(store) = store().await else { return };
    let project_id = project(&store).await;

    let admitted = store
        .admit_create_sandbox(&request(
            project_id,
            "first-request-key-01",
            json!({"vcpu": 2}),
        ))
        .await
        .expect("admit");

    assert!(
        matches!(admitted, Admission::Admitted { .. }),
        "{admitted:?}"
    );
    cleanup(&store, project_id).await;
}

#[tokio::test]
async fn an_identical_retry_returns_the_original() {
    let Some(store) = store().await else { return };
    let project_id = project(&store).await;
    let req = request(project_id, "identical-retry-key-1", json!({"vcpu": 2}));

    let Admission::Admitted {
        sandbox_id,
        operation_id,
    } = store.admit_create_sandbox(&req).await.expect("first")
    else {
        panic!("first request was not admitted");
    };

    let second = store.admit_create_sandbox(&req).await.expect("retry");
    assert_eq!(
        second,
        Admission::Existing {
            sandbox_id,
            operation_id,
            status: "queued".to_owned(),
        },
        "a retry created new work instead of returning the original"
    );

    let sandboxes: (i64,) = sqlx::query_as("SELECT count(*) FROM sandboxes WHERE project_id = $1")
        .bind(project_id.uuid())
        .fetch_one(store.pool())
        .await
        .expect("count");
    assert_eq!(sandboxes.0, 1, "a retry created a second sandbox");

    cleanup(&store, project_id).await;
}

#[tokio::test]
async fn field_order_alone_is_still_the_same_request() {
    let Some(store) = store().await else { return };
    let project_id = project(&store).await;

    let first = request(
        project_id,
        "reordered-payload-key",
        json!({"vcpu": 2, "name": "x"}),
    );
    let reordered = request(
        project_id,
        "reordered-payload-key",
        json!({"name": "x", "vcpu": 2}),
    );

    store.admit_create_sandbox(&first).await.expect("first");
    let second = store
        .admit_create_sandbox(&reordered)
        .await
        .expect("second");

    assert!(
        matches!(second, Admission::Existing { .. }),
        "reordering JSON fields was treated as a different request: {second:?}"
    );
    cleanup(&store, project_id).await;
}

#[tokio::test]
async fn a_changed_payload_under_the_same_key_conflicts() {
    let Some(store) = store().await else { return };
    let project_id = project(&store).await;

    let first = request(project_id, "changed-payload-key-1", json!({"vcpu": 2}));
    let changed = request(project_id, "changed-payload-key-1", json!({"vcpu": 4}));

    let Admission::Admitted { operation_id, .. } =
        store.admit_create_sandbox(&first).await.expect("first")
    else {
        panic!("first request was not admitted");
    };

    let second = store.admit_create_sandbox(&changed).await.expect("second");
    assert_eq!(
        second,
        Admission::DigestConflict { operation_id },
        "a different payload was accepted under a used key"
    );

    let sandboxes: (i64,) = sqlx::query_as("SELECT count(*) FROM sandboxes WHERE project_id = $1")
        .bind(project_id.uuid())
        .fetch_one(store.pool())
        .await
        .expect("count");
    assert_eq!(
        sandboxes.0, 1,
        "a conflicting request still created a sandbox"
    );

    cleanup(&store, project_id).await;
}

#[tokio::test]
async fn the_same_key_in_two_projects_is_two_requests() {
    let Some(store) = store().await else { return };
    let a = project(&store).await;
    let b = project(&store).await;

    let first = store
        .admit_create_sandbox(&request(a, "shared-across-projects", json!({})))
        .await
        .expect("project a");
    let second = store
        .admit_create_sandbox(&request(b, "shared-across-projects", json!({})))
        .await
        .expect("project b");

    assert!(matches!(first, Admission::Admitted { .. }));
    assert!(matches!(second, Admission::Admitted { .. }));

    cleanup(&store, a).await;
    cleanup(&store, b).await;
}

#[tokio::test]
async fn concurrent_identical_requests_admit_exactly_one() {
    let Some(store) = store().await else { return };
    let project_id = project(&store).await;
    let req = request(project_id, "concurrent-race-key-01", json!({"vcpu": 2}));

    // Eight at once, all identical. Whichever loses the unique constraint must
    // read the winner's row rather than failing or inserting a second sandbox.
    let mut set = tokio::task::JoinSet::new();
    for _ in 0..8 {
        let store = store.clone();
        let req = req.clone();
        set.spawn(async move { store.admit_create_sandbox(&req).await });
    }

    let mut admitted = 0;
    let mut existing = 0;
    while let Some(result) = set.join_next().await {
        match result.expect("task").expect("admit") {
            Admission::Admitted { .. } => admitted += 1,
            Admission::Existing { .. } => existing += 1,
            other => panic!("unexpected outcome under concurrency: {other:?}"),
        }
    }

    assert_eq!(admitted, 1, "more than one concurrent request was admitted");
    assert_eq!(existing, 7);

    let sandboxes: (i64,) = sqlx::query_as("SELECT count(*) FROM sandboxes WHERE project_id = $1")
        .bind(project_id.uuid())
        .fetch_one(store.pool())
        .await
        .expect("count");
    assert_eq!(sandboxes.0, 1, "concurrency produced more than one sandbox");

    cleanup(&store, project_id).await;
}
