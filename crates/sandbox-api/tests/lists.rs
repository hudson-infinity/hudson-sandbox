//! Tenant-scoped collection reads and pagination through the actual HTTP router.
#![allow(clippy::unwrap_used, clippy::expect_used)]
use axum::{
    Router,
    body::{Body, to_bytes},
};
use http::{Request, StatusCode, header};
use sandbox_api::{AppState, router};
use sandbox_protocol::{
    Id, OperationId, ProjectId, ProjectToken, SandboxId, images::ImageAllowlist,
};
use sandbox_store::Store;
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt;

struct Fixture {
    app: Router,
    projects: Vec<ProjectId>,
    tokens: Vec<String>,
}
impl Fixture {
    async fn new(pool: &PgPool) -> Self {
        let mut projects = Vec::new();
        let mut tokens = Vec::new();
        for _ in 0..2 {
            let project = ProjectId::generate();
            let token = ProjectToken::generate().unwrap();
            sqlx::query("INSERT INTO projects(id,name,status,limits,api_tokens) VALUES($1,'lists','active','{}',$2)")
                .bind(project.uuid()).bind(json!([{"key_id":token.key_id().as_str(),"hash":hex::encode(token.hash().as_bytes())}])).execute(pool).await.unwrap();
            projects.push(project);
            tokens.push(token.render_once());
        }
        Self {
            app: router(AppState {
                store: Store::from_pool(pool.clone()),
                images: ImageAllowlist::new([format!("sha256:{}", "a".repeat(64))]).unwrap(),
            }),
            projects,
            tokens,
        }
    }
    async fn send(
        &self,
        who: Option<usize>,
        method: &str,
        path: &str,
        body: Value,
    ) -> (StatusCode, Value, http::HeaderMap) {
        let mut request = Request::builder()
            .method(method)
            .uri(path)
            .header(header::CONTENT_TYPE, "application/json")
            .header("idempotency-key", OperationId::generate().to_string());
        if let Some(who) = who {
            request = request.header(
                header::AUTHORIZATION,
                format!("Bearer {}", self.tokens[who]),
            );
        }
        let response = self
            .app
            .clone()
            .oneshot(request.body(Body::from(body.to_string())).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = to_bytes(response.into_body(), 2 * 1024 * 1024)
            .await
            .unwrap();
        (status, serde_json::from_slice(&bytes).unwrap(), headers)
    }
    async fn get(&self, who: usize, path: &str) -> (StatusCode, Value, http::HeaderMap) {
        self.send(Some(who), "GET", path, Value::Null).await
    }
    async fn create(&self, who: usize) -> (SandboxId, OperationId) {
        let (status, body, _) = self
            .send(
                Some(who),
                "POST",
                "/v1/sandboxes",
                json!({"image_digest":format!("sha256:{}","a".repeat(64)),
            "resources":{"vcpu":1,"memory_mib":512,"disk_mib":1024}}),
            )
            .await;
        assert_eq!(status, StatusCode::ACCEPTED, "{body}");
        (
            body["sandbox_id"].as_str().unwrap().parse().unwrap(),
            body["operation_id"].as_str().unwrap().parse().unwrap(),
        )
    }
}
fn ids(body: &Value, field: &str) -> Vec<String> {
    body["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v[field].as_str().unwrap().to_owned())
        .collect()
}
fn cursor(body: &Value) -> String {
    body["next_cursor"].as_str().unwrap().to_owned()
}
fn change_cursor(value: &str, change: impl FnOnce(&mut Value)) -> String {
    let bytes = hex::decode(value.strip_prefix("v1.").unwrap()).unwrap();
    let mut value: Value = serde_json::from_slice(&bytes).unwrap();
    change(&mut value);
    format!("v1.{}", hex::encode(serde_json::to_vec(&value).unwrap()))
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn empty_collections_authenticate_and_get_coexists_with_create(pool: PgPool) {
    let f = Fixture::new(&pool).await;
    for path in ["/v1/sandboxes", "/v1/operations"] {
        let (status, body, headers) = f.get(0, path).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, json!({"items":[],"next_cursor":null}));
        assert_eq!(headers[header::CACHE_CONTROL], "no-store");
        assert_eq!(headers[header::CONTENT_TYPE], "application/json");
        assert_eq!(
            f.send(None, "GET", path, Value::Null).await.0,
            StatusCode::UNAUTHORIZED
        );
    }
    let (sandbox, operation) = f.create(0).await;
    let (_, body, _) = f.get(0, "/v1/sandboxes").await;
    assert_eq!(ids(&body, "sandbox_id"), [sandbox.to_string()]);
    let (_, body, _) = f.get(0, "/v1/operations").await;
    assert_eq!(ids(&body, "operation_id"), [operation.to_string()]);
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn timestamp_ties_and_new_inserts_do_not_shift_existing_pages(pool: PgPool) {
    let f = Fixture::new(&pool).await;
    let mut expected = Vec::new();
    for _ in 0..7 {
        expected.push(f.create(0).await.0.to_string());
    }
    f.create(1).await;
    sqlx::query("UPDATE sandboxes SET created_at='2026-01-01T00:00:00.123456Z'")
        .execute(&pool)
        .await
        .unwrap();
    expected.sort();
    expected.reverse();
    let (status, first, _) = f.get(0, "/v1/sandboxes?limit=2").await;
    assert_eq!(status, StatusCode::OK);
    let mut seen = ids(&first, "sandbox_id");
    let mut next = first["next_cursor"].clone();
    let newest = f.create(0).await.0.to_string();
    while let Some(value) = next.as_str() {
        let (status, body, _) = f
            .get(0, &format!("/v1/sandboxes?limit=3&cursor={value}"))
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        seen.extend(ids(&body, "sandbox_id"));
        next = body["next_cursor"].clone();
    }
    assert_eq!(seen, expected);
    assert!(!seen.contains(&newest));
    let (_, fresh, _) = f.get(0, "/v1/sandboxes?limit=1").await;
    assert_eq!(ids(&fresh, "sandbox_id"), [newest]);
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn operation_filter_paginates_service_cleanup_and_matches_point_reads(pool: PgPool) {
    let f = Fixture::new(&pool).await;
    let (sandbox, create) = f.create(0).await;
    let (_, other) = f.create(0).await;
    f.create(1).await;
    let service = OperationId::generate();
    sqlx::query("INSERT INTO operations(id,project_id,sandbox_id,kind,initiator_kind,idempotency_key,request_digest,digest_version,payload,status,phase,error)
        VALUES($1,$2,$3,'destroy','service',$4,$5,1,'{}','unknown','release_unconfirmed','{\"code\":\"release_unconfirmed\"}')")
        .bind(service.uuid()).bind(f.projects[0].uuid()).bind(sandbox.uuid()).bind(service.to_string()).bind(vec![0u8;32]).execute(&pool).await.unwrap();
    sqlx::query("UPDATE operations SET created_at='2026-01-01T00:00:00.123456Z'")
        .execute(&pool)
        .await
        .unwrap();
    let mut expected = vec![create.to_string(), service.to_string()];
    expected.sort();
    expected.reverse();
    let mut seen = Vec::new();
    let mut path = format!("/v1/operations?sandbox_id={sandbox}&limit=1");
    loop {
        let (status, body, _) = f.get(0, &path).await;
        assert_eq!(status, StatusCode::OK);
        for item in body["items"].as_array().unwrap() {
            let id = item["operation_id"].as_str().unwrap();
            let (_, point, _) = f.get(0, &format!("/v1/operations/{id}")).await;
            assert_eq!(item, &point);
            if id == service.to_string() {
                assert_eq!(item["status"], "unknown");
                assert!(item.get("completed_at").is_none());
            }
        }
        seen.extend(ids(&body, "operation_id"));
        let Some(next) = body["next_cursor"].as_str() else {
            break;
        };
        path = format!("/v1/operations?sandbox_id={sandbox}&limit=1&cursor={next}");
    }
    assert_eq!(seen, expected);
    assert!(!seen.contains(&other.to_string()));
    let (_, all, _) = f.get(0, "/v1/operations").await;
    assert_eq!(all["items"].as_array().unwrap().len(), 3);
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn cursor_scope_cannot_be_mixed_across_tenants_collections_or_filters(pool: PgPool) {
    let f = Fixture::new(&pool).await;
    let (first, _) = f.create(0).await;
    let (second, _) = f.create(0).await;
    f.create(1).await;
    let (_, sandboxes, _) = f.get(0, "/v1/sandboxes?limit=1").await;
    let sc = cursor(&sandboxes);
    for (who, path) in [
        (1, format!("/v1/sandboxes?cursor={sc}")),
        (0, format!("/v1/operations?cursor={sc}")),
    ] {
        let (status, body, _) = f.get(who, &path).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["code"], "bad_request");
    }
    let (_, operations, _) = f.get(0, "/v1/operations?limit=1").await;
    let oc = cursor(&operations);
    assert_eq!(
        f.get(0, &format!("/v1/operations?sandbox_id={first}&cursor={oc}"))
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
    let filtered = change_cursor(&oc, |c| c["sandbox"] = json!(first.to_string()));
    assert_eq!(
        f.get(
            0,
            &format!("/v1/operations?sandbox_id={second}&cursor={filtered}")
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        f.get(0, &format!("/v1/operations?cursor={filtered}"))
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn changing_cursor_metadata_never_grants_another_projects_rows(pool: PgPool) {
    let f = Fixture::new(&pool).await;
    let a = f.create(0).await.0;
    f.create(0).await;
    let b = f.create(1).await.0;
    let (_, page, _) = f.get(0, "/v1/sandboxes?limit=1").await;
    // Cursors are positions, not signed credentials. Even a caller-built valid
    // position and scope must go through the SQL project's authorization filter.
    let forged = change_cursor(&cursor(&page), |c| {
        c["project"] = json!(f.projects[1].to_string());
        c["created_micros"] = json!(4102444800000000i64);
        c["id"] = json!(a.to_string());
    });
    let (status, body, _) = f.get(1, &format!("/v1/sandboxes?cursor={forged}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(ids(&body, "sandbox_id"), [b.to_string()]);
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn missing_and_inaccessible_filters_are_indistinguishable(pool: PgPool) {
    let f = Fixture::new(&pool).await;
    let foreign = f.create(1).await.0;
    let (a, body_a, _) = f
        .get(0, &format!("/v1/operations?sandbox_id={foreign}"))
        .await;
    let (b, body_b, _) = f
        .get(
            0,
            &format!("/v1/operations?sandbox_id={}", SandboxId::generate()),
        )
        .await;
    assert_eq!(a, StatusCode::NOT_FOUND);
    assert_eq!(a, b);
    assert_eq!(body_a, body_b);
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn revocation_is_checked_again_on_each_page(pool: PgPool) {
    let f = Fixture::new(&pool).await;
    f.create(0).await;
    f.create(0).await;
    let (_, page, _) = f.get(0, "/v1/sandboxes?limit=1").await;
    sqlx::query("UPDATE projects SET api_tokens='[]' WHERE id=$1")
        .bind(f.projects[0].uuid())
        .execute(&pool)
        .await
        .unwrap();
    let (status, body, _) = f
        .get(0, &format!("/v1/sandboxes?cursor={}", cursor(&page)))
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(body.get("items").is_none());
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn malformed_query_and_cursor_inputs_are_bounded_problem_responses(pool: PgPool) {
    let f = Fixture::new(&pool).await;
    f.create(0).await;
    f.create(0).await;
    let (_, page, _) = f.get(0, "/v1/sandboxes?limit=1").await;
    let valid = cursor(&page);
    let cursors = vec![
        "".into(),
        "not-hex".into(),
        "v2.00".into(),
        "v1.5b5d".into(),
        "x".repeat(2049),
        change_cursor(&valid, |c| c["version"] = json!(2)),
        change_cursor(&valid, |c| c["created_micros"] = json!(i64::MAX)),
        change_cursor(&valid, |c| {
            c["id"] = json!(OperationId::generate().to_string())
        }),
        change_cursor(&valid, |c| c["extra"] = json!(true)),
    ];
    let mut paths = cursors
        .iter()
        .map(|c| format!("/v1/sandboxes?cursor={c}"))
        .collect::<Vec<_>>();
    for query in [
        "limit=0",
        "limit=101",
        "limit=-1",
        "limit=65536",
        "limit=abc",
        "limit=1&limit=2",
        "cursor=a&cursor=b",
        "state=running",
        "sandbox_id=bad",
    ] {
        paths.push(format!("/v1/sandboxes?{query}"));
    }
    paths.push("/v1/operations?sandbox_id=bad".into());
    for path in paths {
        let (status, body, headers) = f.get(0, &path).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{path}: {body}");
        assert_eq!(body["code"], "bad_request");
        assert_eq!(headers[header::CONTENT_TYPE], "application/problem+json");
        assert_eq!(headers[header::CACHE_CONTROL], "no-store");
    }
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn page_size_defaults_and_ceiling_are_enforced(pool: PgPool) {
    let f = Fixture::new(&pool).await;
    for _ in 0..101 {
        f.create(0).await;
    }
    for collection in ["sandboxes", "operations"] {
        let (_, default, _) = f.get(0, &format!("/v1/{collection}")).await;
        assert_eq!(default["items"].as_array().unwrap().len(), 50);
        let (_, maximum, _) = f.get(0, &format!("/v1/{collection}?limit=100")).await;
        assert_eq!(maximum["items"].as_array().unwrap().len(), 100);
        let (_, last, _) = f
            .get(
                0,
                &format!("/v1/{collection}?limit=100&cursor={}", cursor(&maximum)),
            )
            .await;
        assert_eq!(last["items"].as_array().unwrap().len(), 1);
        assert!(last["next_cursor"].is_null());
    }
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn lists_preserve_tombstones_and_provenance_without_mutating_work(pool: PgPool) {
    let f = Fixture::new(&pool).await;
    let (sandbox, operation) = f.create(0).await;
    sqlx::query("UPDATE sandboxes SET observed_state='destroyed',desired_state='destroyed',destroyed_at=clock_timestamp(),observation_simulated=true,active_transition_operation_id=NULL").execute(&pool).await.unwrap();
    sqlx::query("UPDATE operations SET status='failed',completed_at=clock_timestamp(),error='{\"code\":\"allocation_released\",\"simulated\":true}'").execute(&pool).await.unwrap();
    let (_, sandboxes, _) = f.get(0, "/v1/sandboxes").await;
    let (_, point, _) = f.get(0, &format!("/v1/sandboxes/{sandbox}")).await;
    assert_eq!(sandboxes["items"][0], point);
    assert_eq!(point["observed_state"], "destroyed");
    assert_eq!(point["observation_simulated"], true);
    let (_, operations, _) = f.get(0, "/v1/operations").await;
    let (_, point, _) = f.get(0, &format!("/v1/operations/{operation}")).await;
    assert_eq!(operations["items"][0], point);
    let (attempts,revision,count):(i64,i64,i64)=sqlx::query_as("SELECT sum(attempt_count)::bigint,(SELECT sum(state_revision)::bigint FROM sandboxes),count(*) FROM operations").fetch_one(&pool).await.unwrap();
    assert_eq!((attempts, revision, count), (0, 0, 1));
}
