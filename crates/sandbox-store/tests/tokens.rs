//! Token lookup against a real PostgreSQL.
//!
//! Set `DATABASE_URL` to run these (`make up` provides one). Unlike the schema
//! tests these commit, because the lookup runs on the pool rather than inside
//! a caller's transaction, so each one cleans up the project it created.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use sandbox_protocol::{Id, ProjectId, ProjectToken};
use sandbox_store::Store;
use sandbox_store::projects::ProjectStatus;
use serde_json::json;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

async fn store() -> Option<Store> {
    let Ok(url) = std::env::var("DATABASE_URL") else {
        eprintln!("skipping: DATABASE_URL is unset — run `make up` and export it");
        return None;
    };
    let store = Store::connect(&url, 5).await.expect("connect");
    store.migrate().await.expect("migrate");
    Some(store)
}

/// Insert a project carrying one token, and return its id.
async fn project_with_token(
    store: &Store,
    token: &ProjectToken,
    status: &str,
    expires_at: Option<OffsetDateTime>,
    revoked_at: Option<OffsetDateTime>,
) -> ProjectId {
    let id = ProjectId::generate();
    let stamp =
        |at: Option<OffsetDateTime>| at.map(|at| at.format(&Rfc3339).expect("format timestamp"));

    let tokens = json!([{
        "key_id": token.key_id().as_str(),
        "hash": hex::encode(token.hash().as_bytes()),
        "expires_at": stamp(expires_at),
        "revoked_at": stamp(revoked_at),
    }]);

    sqlx::query(
        "INSERT INTO projects (id, name, status, limits, api_tokens)
         VALUES ($1, 'token-test', $2, '{}', $3)",
    )
    .bind(id.uuid())
    .bind(status)
    .bind(&tokens)
    .execute(store.pool())
    .await
    .expect("insert project");

    id
}

async fn delete_project(store: &Store, id: ProjectId) {
    sqlx::query("DELETE FROM projects WHERE id = $1")
        .bind(id.uuid())
        .execute(store.pool())
        .await
        .expect("cleanup");
}

#[tokio::test]
async fn finds_a_token_and_its_project() {
    let Some(store) = store().await else { return };
    let token = ProjectToken::generate().expect("generate");
    let project = project_with_token(&store, &token, "active", None, None).await;

    let found = store
        .token_by_key_id(token.key_id())
        .await
        .expect("lookup")
        .expect("token exists");

    assert_eq!(found.project_id, project);
    assert_eq!(found.project_status, ProjectStatus::Active);
    assert_eq!(&found.key_id, token.key_id());
    assert!(
        found.hash.verify(&token.hash()),
        "stored hash did not match the token it was created from"
    );
    assert!(found.expires_at.is_none());
    assert!(found.revoked_at.is_none());

    delete_project(&store, project).await;
}

#[tokio::test]
async fn an_unknown_key_id_is_absent_rather_than_an_error() {
    let Some(store) = store().await else { return };
    let token = ProjectToken::generate().expect("generate");

    let found = store.token_by_key_id(token.key_id()).await.expect("lookup");
    assert!(found.is_none(), "an unissued key identifier was found");
}

#[tokio::test]
async fn carries_expiry_and_revocation_through() {
    let Some(store) = store().await else { return };
    let token = ProjectToken::generate().expect("generate");
    let expires = OffsetDateTime::now_utc() + time::Duration::hours(1);
    let revoked = OffsetDateTime::now_utc() - time::Duration::minutes(5);
    let project =
        project_with_token(&store, &token, "suspended", Some(expires), Some(revoked)).await;

    let found = store
        .token_by_key_id(token.key_id())
        .await
        .expect("lookup")
        .expect("token exists");

    assert_eq!(found.project_status, ProjectStatus::Suspended);
    // Whole seconds: PostgreSQL stores microseconds, the comparison only needs
    // to prove the value survived the round trip.
    assert_eq!(
        found.expires_at.map(OffsetDateTime::unix_timestamp),
        Some(expires.unix_timestamp())
    );
    assert_eq!(
        found.revoked_at.map(OffsetDateTime::unix_timestamp),
        Some(revoked.unix_timestamp())
    );

    delete_project(&store, project).await;
}
