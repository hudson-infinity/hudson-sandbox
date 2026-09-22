//! Actual router/PostgreSQL exchanges; byte backends are synthetic, not VM evidence.
#![allow(clippy::unwrap_used, clippy::expect_used)]
#[path = "support/contract_backends.rs"]
mod backends;
#[path = "support/output.rs"]
mod fixture;
use axum::{
    Router,
    body::{Body, to_bytes},
};
use fixture::Fixture;
use http::Request;
use sandbox_protocol::{Id, OperationId};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use std::{
    io::Write,
    process::{Command, Stdio},
    sync::Arc,
};
use tower::ServiceExt;

fn now() -> i64 {
    (time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000) as i64
}
struct Session {
    app: Router,
    token: String,
    exchanges: Vec<Value>,
}
impl Session {
    async fn send(
        &mut self,
        method: &str,
        uri: &str,
        body: Vec<u8>,
        headers: &[(&str, &str)],
        expected: u16,
    ) -> Value {
        let mut request = Request::builder()
            .method(method)
            .uri(uri)
            .header("authorization", format!("Bearer {}", self.token));
        let mut recorded = serde_json::Map::new();
        for (k, v) in headers {
            request = request.header(*k, *v);
            recorded.insert(k.to_lowercase(), json!(v));
        }
        let request_json = if recorded.get("content-type") == Some(&json!("application/json")) {
            serde_json::from_slice::<Value>(&body).unwrap()
        } else {
            Value::Null
        };
        let response = self
            .app
            .clone()
            .oneshot(request.body(Body::from(body)).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), expected, "{method} {uri}");
        let response_headers: serde_json::Map<String, Value> = response
            .headers()
            .iter()
            .map(|(k, v)| (k.to_string(), json!(v.to_str().unwrap())))
            .collect();
        let media = response
            .headers()
            .get("content-type")
            .map(|v| v.to_str().unwrap().to_owned())
            .unwrap_or_default();
        let bytes = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            to_bytes(response.into_body(), 131072),
        )
        .await
        .unwrap()
        .unwrap();
        let response_json = if media.contains("json") {
            serde_json::from_slice::<Value>(&bytes).unwrap()
        } else {
            Value::Null
        };
        self.exchanges.push(json!({"method":method,"uri":uri,"status":expected,"request_headers":recorded,"request_json":request_json,"response_headers":response_headers,"response_bytes":bytes.len(),"response_json":response_json,"response_text":if media.starts_with("text/event-stream"){String::from_utf8(bytes.to_vec()).unwrap()}else{String::new()}}));
        response_json
    }
    async fn json(&mut self, method: &str, uri: &str, body: Value, expected: u16) -> Value {
        let key = OperationId::generate().to_string();
        self.send(
            method,
            uri,
            serde_json::to_vec(&body).unwrap(),
            &[
                ("content-type", "application/json"),
                ("idempotency-key", &key),
            ],
            expected,
        )
        .await
    }
    async fn get(&mut self, uri: &str, expected: u16) -> Value {
        self.send("GET", uri, vec![], &[], expected).await
    }
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn all_implemented_operations_match_the_versioned_contract(pool: PgPool) {
    let f = Fixture::new(&pool).await;
    // This older output fixture intentionally uses minimal metadata. Contract
    // evidence uses the same canonical image/resources as actual admission.
    let digest = format!("sha256:{}", "a".repeat(64));
    sqlx::query(
        "UPDATE sandboxes SET image_digest=$1,resources=$2,observation_simulated=true WHERE id=$3",
    )
    .bind(&digest)
    .bind(json!({"vcpu":1,"memory_mib":128,"disk_mib":64}))
    .bind(f.sandbox.uuid())
    .execute(&pool)
    .await
    .unwrap();
    f.publish().await;
    let backend = Arc::new(backends::Backend);
    let app = sandbox_api::router_with_uploads(
        sandbox_api::AppState {
            store: f.store.clone(),
            images: sandbox_protocol::images::ImageAllowlist::new([digest.clone()]).unwrap(),
        },
        Some(backend.clone()),
        None,
        Some(backend.clone()),
        Some(backend),
    );
    let mut s = Session {
        app,
        token: f.token.clone(),
        exchanges: vec![],
    };
    let sb = format!("/v1/sandboxes/{}", f.sandbox);
    let op = format!("/v1/operations/{}", f.operation);
    s.get(&sb, 200).await;
    s.get("/v1/sandboxes/%FF", 400).await;
    s.get("/v1/operations/%FF", 400).await;
    s.get("/v1/operations/%FF/stream", 400).await;
    s.json(
        "POST",
        "/v1/sandboxes/%FF/execute",
        json!({"argv":["/bin/true"],"deadline_unix_ms":now()+60000}),
        400,
    )
    .await;
    s.json("POST", "/v1/sandboxes/%FF/destroy", json!({}), 400)
        .await;
    s.json("POST", "/v1/operations/%FF/cancel", json!({}), 400)
        .await;
    s.get(&op, 200).await;
    s.get("/v1/sandboxes?limit=1", 200).await;
    s.get(
        &format!("/v1/operations?sandbox_id={}&limit=1", f.sandbox),
        200,
    )
    .await;
    s.get(&format!("{op}/outputs/stdout?offset=0&limit=2"), 200)
        .await;
    s.get(&format!("{op}/outputs/stderr"), 200).await;
    s.get(&format!("{op}/outputs/stdout?offset=99"), 416).await;
    s.get(&format!("{op}/stream"), 200).await;
    s.json("POST", &format!("{op}/cancel"), json!({}), 202)
        .await;
    let capture_route = format!("{sb}/files/captures");
    let captured = s
        .json("POST", &capture_route, json!({"path":"hello.bin"}), 201)
        .await;
    let capture = captured["capture"].as_str().unwrap();
    s.send(
        "GET",
        &format!("{capture_route}?offset=0&limit=2"),
        vec![],
        &[("x-file-capture", capture)],
        200,
    )
    .await;
    s.send(
        "DELETE",
        &capture_route,
        vec![],
        &[("x-file-capture", capture)],
        204,
    )
    .await;
    let bytes = b"hello\0\xff";
    let sha = hex::encode(Sha256::digest(bytes));
    let size = bytes.len().to_string();
    let key = OperationId::generate().to_string();
    let upload = s
        .send(
            "PUT",
            &format!("{sb}/files?path=hello.bin"),
            bytes.to_vec(),
            &[
                ("content-type", "application/octet-stream"),
                ("x-file-size", &size),
                ("x-file-sha256", &sha),
                ("x-file-mode", "0755"),
                ("idempotency-key", &key),
            ],
            202,
        )
        .await;
    s.get(upload["status_url"].as_str().unwrap(), 200).await;
    // A separate sandbox allows command admission while upload remains queued.
    let g = Fixture::new(&pool).await;
    g.finish().await;
    sqlx::query(
        "UPDATE sandboxes SET image_digest=$1,resources=$2,observation_simulated=true WHERE id=$3",
    )
    .bind(&digest)
    .bind(json!({"vcpu":1,"memory_mib":128,"disk_mib":64}))
    .bind(g.sandbox.uuid())
    .execute(&pool)
    .await
    .unwrap();
    s.token = g.token;
    let execute = s
        .json(
            "POST",
            &format!("/v1/sandboxes/{}/execute", g.sandbox),
            json!({"argv":["/bin/true"],"deadline_unix_ms":now()+60000}),
            202,
        )
        .await;
    s.get(execute["status_url"].as_str().unwrap(), 200).await;
    s.json(
        "POST",
        &format!("/v1/sandboxes/{}/destroy", g.sandbox),
        json!({}),
        202,
    )
    .await;
    s.json(
        "POST",
        "/v1/sandboxes",
        json!({"image_digest":digest,"resources":{"vcpu":1,"memory_mib":128,"disk_mib":64}}),
        202,
    )
    .await;
    s.json(
        "POST",
        "/v1/sandboxes",
        json!({"image_digest":"latest","resources":{"vcpu":1,"memory_mib":128,"disk_mib":64}}),
        400,
    )
    .await;
    s.get(&sb, 404).await; // real cross-project response
    s.get("/v1/sandboxes?limit=0", 400).await;
    s.token = f.token.clone();
    sqlx::query("UPDATE operations SET response_expires_at=clock_timestamp()-interval '1 second' WHERE id=$1")
        .bind(f.operation.uuid()).execute(&pool).await.unwrap();
    s.get(&op, 410).await;
    s.get("/v1/operations", 200).await;
    s.token = "invalid".into();
    s.get("/v1/sandboxes", 401).await;
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let python = std::env::var_os("HUDSON_OPENAPI_PYTHON")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| root.join(".venv-openapi/bin/python"));
    let mut child = Command::new(python)
        .arg(root.join("scripts/check_api.py"))
        .arg("--exchanges")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("install contract validation tools with make api-setup");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(&serde_json::to_vec(&s.exchanges).unwrap())
        .unwrap();
    let result = child.wait_with_output().unwrap();
    assert!(
        result.status.success(),
        "{}{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
    println!("{}", String::from_utf8_lossy(&result.stdout));
}
