//! Public authorization and race evidence with real PostgreSQL and a controlled reader.
#![allow(clippy::unwrap_used, clippy::expect_used)]
#[path = "support/output.rs"]
mod fixture;
use axum::{
    Router,
    body::{Body, to_bytes},
};
use fixture::Fixture;
use http::{HeaderMap, Request, StatusCode};
use sandbox_api::{
    files::client::{FileReader, Reply},
    problem::Problem,
};
use sandbox_protocol::{
    HostId, Id, OperationId, ProjectToken,
    file_downloads::ReadScope,
    guest,
    supervisor::{
        FileAccessObservation, FileCaptureRequest, FileDownloadHandle, FileDownloadRequest,
        FileReleaseRequest, file_access_observation::Result as ResultBody,
    },
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use std::{
    collections::{HashMap, HashSet},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::{Mutex, Notify, Semaphore};
use tower::ServiceExt;
static SERIAL: Mutex<()> = Mutex::const_new(());
fn now() -> i64 {
    (time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000) as i64
}
#[derive(Debug)]
struct Reader {
    data: Vec<u8>,
    handles: Mutex<HashMap<String, FileDownloadHandle>>,
    released: Mutex<HashSet<String>>,
    calls: AtomicUsize,
    pause: AtomicBool,
    started: Notify,
    go: Semaphore,
    corrupt: AtomicBool,
    fail: AtomicBool,
}
impl Reader {
    fn new(data: Vec<u8>) -> Self {
        Self {
            data,
            handles: Mutex::new(HashMap::new()),
            released: Mutex::new(HashSet::new()),
            calls: AtomicUsize::new(0),
            pause: AtomicBool::new(false),
            started: Notify::new(),
            go: Semaphore::new(0),
            corrupt: AtomicBool::new(false),
            fail: AtomicBool::new(false),
        }
    }
    async fn step(&self) -> Result<(), Problem> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.started.notify_one();
        if self.pause.load(Ordering::SeqCst) {
            self.go.acquire().await.unwrap().forget();
        }
        if self.fail.load(Ordering::SeqCst) {
            return Err(Problem::Unavailable);
        }
        Ok(())
    }
    fn result(&self, scope_json: Vec<u8>, result: ResultBody) -> FileAccessObservation {
        FileAccessObservation {
            scope_json,
            simulated: true,
            observed_unix_ms: now(),
            result: Some(result),
        }
    }
}
impl FileReader for Reader {
    fn capture(&self, host: HostId, r: FileCaptureRequest) -> Reply<'_> {
        Box::pin(async move {
            self.step().await?;
            let s: ReadScope = serde_json::from_slice(&r.scope_json).unwrap();
            assert_eq!(s.host_id, host);
            if r.path == "missing" {
                return Err(Problem::FileMissing);
            }
            let h = FileDownloadHandle {
                id: OperationId::generate().to_string(),
                context: Some(guest::Context {
                    allocation_id: s.allocation_id.to_string(),
                    generation: s.generation,
                    boot_id: "test-boot".into(),
                }),
                capture: Some(guest::FileCapture {
                    capture_id: OperationId::generate().to_string(),
                    path: r.path,
                    size: self.data.len() as u64,
                    sha256: Sha256::digest(&self.data).to_vec(),
                    expires_unix_ms: now() + 60000,
                }),
                expires_unix_ms: now() + 60000,
            };
            self.handles.lock().await.insert(h.id.clone(), h.clone());
            let mut out = self.result(r.scope_json, ResultBody::Captured(h));
            if self.corrupt.load(Ordering::SeqCst) {
                out.simulated = false;
            }
            Ok(out)
        })
    }
    fn read(&self, host: HostId, r: FileDownloadRequest) -> Reply<'_> {
        Box::pin(async move {
            self.step().await?;
            let s: ReadScope = serde_json::from_slice(&r.scope_json).unwrap();
            assert_eq!(s.host_id, host);
            let h = r.handle.unwrap();
            if self.handles.lock().await.get(&h.id) != Some(&h)
                || self.released.lock().await.contains(&h.id)
            {
                return Err(Problem::FileMissing);
            }
            let c = h.capture.unwrap();
            let next = (r.offset + r.limit as u64).min(c.size);
            let chunk = guest::FileChunk {
                handle: Some(guest::FileHandle {
                    capture_id: c.capture_id,
                    sha256: c.sha256,
                }),
                offset: r.offset,
                data: self.data[r.offset as usize..next as usize].to_vec(),
                next_offset: next + u64::from(self.corrupt.load(Ordering::SeqCst)),
                size: c.size,
                at_end: next == c.size,
            };
            Ok(self.result(r.scope_json, ResultBody::Chunk(chunk)))
        })
    }
    fn release(&self, _: HostId, r: FileReleaseRequest) -> Reply<'_> {
        Box::pin(async move {
            self.step().await?;
            let h = r.handle.unwrap();
            if self.handles.lock().await.get(&h.id) != Some(&h) {
                return Err(Problem::FileMissing);
            }
            self.released.lock().await.insert(h.id.clone());
            Ok(self.result(r.scope_json, ResultBody::Released(h)))
        })
    }
}
fn route(f: &Fixture) -> String {
    format!("/v1/sandboxes/{}/files/captures", f.sandbox)
}
async fn setup(pool: &PgPool, data: Vec<u8>) -> (Fixture, Arc<Reader>, Router) {
    let f = Fixture::new(pool).await;
    sqlx::query("UPDATE sandboxes SET observation_simulated=true WHERE id=$1")
        .bind(f.sandbox.uuid())
        .execute(pool)
        .await
        .unwrap();
    let reader = Arc::new(Reader::new(data));
    let app = sandbox_api::router_with_files(
        sandbox_api::AppState {
            store: f.store.clone(),
            images: sandbox_protocol::images::ImageAllowlist::new([format!(
                "sha256:{}",
                "a".repeat(64)
            )])
            .unwrap(),
        },
        None,
        None,
        Some(reader.clone()),
    );
    (f, reader, app)
}
async fn call(
    app: Router,
    token: &str,
    method: &str,
    url: &str,
    capture: Option<&str>,
    body: Value,
) -> (StatusCode, HeaderMap, Vec<u8>) {
    let mut r = Request::builder()
        .method(method)
        .uri(url)
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/json");
    if let Some(c) = capture {
        r = r.header("x-file-capture", c);
    }
    let body = if body.is_null() {
        Body::empty()
    } else {
        Body::from(body.to_string())
    };
    let response = app.oneshot(r.body(body).unwrap()).await.unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let body = to_bytes(response.into_body(), 65536)
        .await
        .unwrap()
        .to_vec();
    (status, headers, body)
}
async fn capture(app: &Router, f: &Fixture) -> String {
    let r = call(
        app.clone(),
        &f.token,
        "POST",
        &route(f),
        None,
        json!({"path":"file.bin"}),
    )
    .await;
    assert_eq!(r.0, StatusCode::CREATED, "{:?}", r);
    serde_json::from_slice::<Value>(&r.2).unwrap()["capture"]
        .as_str()
        .unwrap()
        .into()
}
fn problem(r: &(StatusCode, HeaderMap, Vec<u8>), status: StatusCode, code: &str) {
    assert_eq!(r.0, status, "{}", String::from_utf8_lossy(&r.2));
    assert_eq!(r.1["cache-control"], "no-store");
    assert_eq!(serde_json::from_slice::<Value>(&r.2).unwrap()["code"], code);
}
#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn binary_capture_ranges_release_and_empty_files(pool: PgPool) {
    let _s = SERIAL.lock().await;
    for bytes in [vec![], (0..65539).map(|n| (n % 251) as u8).collect()] {
        let (f, reader, app) = setup(&pool, bytes.clone()).await;
        let token = capture(&app, &f).await;
        let mut output = Vec::new();
        loop {
            let r = call(
                app.clone(),
                &f.token,
                "GET",
                &format!("{}?offset={}&limit=32768", route(&f), output.len()),
                Some(&token),
                Value::Null,
            )
            .await;
            assert_eq!(r.0, StatusCode::OK, "{}", String::from_utf8_lossy(&r.2));
            assert_eq!(r.1["content-type"], "application/octet-stream");
            assert_eq!(r.1["x-content-type-options"], "nosniff");
            assert_eq!(r.1["x-file-simulated"], "true");
            assert_eq!(r.1["x-file-sha256"], hex::encode(Sha256::digest(&bytes)));
            output.extend(r.2);
            if r.1["x-file-eof"] == "true" {
                break;
            }
        }
        assert_eq!(output, bytes);
        for _ in 0..2 {
            assert_eq!(
                call(
                    app.clone(),
                    &f.token,
                    "DELETE",
                    &route(&f),
                    Some(&token),
                    Value::Null
                )
                .await
                .0,
                StatusCode::NO_CONTENT
            );
        }
        problem(
            &call(app, &f.token, "GET", &route(&f), Some(&token), Value::Null).await,
            StatusCode::GONE,
            "file_capture_missing",
        );
        assert_eq!(reader.handles.lock().await.len(), 1);
    }
}
#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn missing_foreign_malformed_and_forged_tokens_do_not_authorize_file_access(pool: PgPool) {
    let _s = SERIAL.lock().await;
    let (f, reader, app) = setup(&pool, b"data".to_vec()).await;
    let token = capture(&app, &f).await;
    problem(
        &call(
            app.clone(),
            "bad",
            "GET",
            &route(&f),
            Some(&token),
            Value::Null,
        )
        .await,
        StatusCode::UNAUTHORIZED,
        "unauthenticated",
    );
    let other = Fixture::new(&pool).await;
    problem(
        &call(
            app.clone(),
            &other.token,
            "GET",
            &route(&f),
            Some(&token),
            Value::Null,
        )
        .await,
        StatusCode::NOT_FOUND,
        "not_found",
    );
    let missing = format!(
        "/v1/sandboxes/{}/files/captures",
        sandbox_protocol::SandboxId::generate()
    );
    problem(
        &call(
            app.clone(),
            &f.token,
            "POST",
            &missing,
            None,
            json!({"path":"file"}),
        )
        .await,
        StatusCode::NOT_FOUND,
        "not_found",
    );
    for suffix in ["?offset=1&offset=2", "?limit=0", "?limit=32769", "?wat=1"] {
        problem(
            &call(
                app.clone(),
                &f.token,
                "GET",
                &format!("{}{suffix}", route(&f)),
                Some(&token),
                Value::Null,
            )
            .await,
            StatusCode::BAD_REQUEST,
            "bad_request",
        );
    }
    for path in ["../secret", "/etc/passwd", "a//b", ".hudson-transfers/x"] {
        problem(
            &call(
                app.clone(),
                &f.token,
                "POST",
                &route(&f),
                None,
                json!({"path":path}),
            )
            .await,
            StatusCode::BAD_REQUEST,
            "bad_request",
        );
    }
    problem(
        &call(
            app.clone(),
            &f.token,
            "POST",
            &format!("{}?wat=1", route(&f)),
            None,
            json!({"path":"file"}),
        )
        .await,
        StatusCode::BAD_REQUEST,
        "bad_request",
    );
    for raw in [None, Some("broken"), Some("e30")] {
        problem(
            &call(app.clone(), &f.token, "GET", &route(&f), raw, Value::Null).await,
            StatusCode::BAD_REQUEST,
            "bad_request",
        );
    }
    problem(
        &call(
            app.clone(),
            &f.token,
            "GET",
            &format!("{}?offset=5", route(&f)),
            Some(&token),
            Value::Null,
        )
        .await,
        StatusCode::RANGE_NOT_SATISFIABLE,
        "file_range_invalid",
    );
    assert_eq!(reader.calls.load(Ordering::SeqCst), 1);
    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD as B64};
    let mut forged: Value = serde_json::from_slice(&B64.decode(&token).unwrap()).unwrap();
    forged["handle"]["capture"]["path"] = "other".into();
    let forged = B64.encode(serde_json::to_vec(&forged).unwrap());
    problem(
        &call(
            app.clone(),
            &f.token,
            "GET",
            &route(&f),
            Some(&forged),
            Value::Null,
        )
        .await,
        StatusCode::GONE,
        "file_capture_missing",
    );
    reader.corrupt.store(true, Ordering::SeqCst);
    problem(
        &call(app, &f.token, "GET", &route(&f), Some(&token), Value::Null).await,
        StatusCode::BAD_GATEWAY,
        "file_response_invalid",
    );
}
#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn authorization_or_allocation_changes_during_host_read_discard_bytes_and_errors(
    pool: PgPool,
) {
    let _s = SERIAL.lock().await;
    for change in [
        "revoke",
        "rotate",
        "suspend",
        "epoch",
        "destroy",
        "lease",
        "error-revoke",
        "token-expiry",
        "sandbox-expiry",
    ] {
        let (f, reader, app) = setup(&pool, b"private bytes".to_vec()).await;
        let token = capture(&app, &f).await;
        // Consume any notification left by the completed capture before pausing read.
        reader.started.notified().await;
        reader.pause.store(true, Ordering::SeqCst);
        let wait = reader.started.notified();
        let a = app.clone();
        let bearer = f.token.clone();
        let url = route(&f);
        let task =
            tokio::spawn(
                async move { call(a, &bearer, "GET", &url, Some(&token), Value::Null).await },
            );
        tokio::time::timeout(Duration::from_secs(3), wait)
            .await
            .unwrap();
        match change {
            "revoke" | "error-revoke" => {
                sqlx::query("UPDATE projects SET api_tokens=jsonb_set(api_tokens,'{0,revoked_at}',to_jsonb(clock_timestamp())) WHERE id=$1").bind(f.project.uuid()).execute(&pool).await.unwrap();
            }
            "token-expiry" => {
                sqlx::query("UPDATE projects SET api_tokens=jsonb_set(api_tokens,'{0,expires_at}',to_jsonb(clock_timestamp())) WHERE id=$1").bind(f.project.uuid()).execute(&pool).await.unwrap();
            }
            "sandbox-expiry" => {
                sqlx::query("UPDATE sandboxes SET expires_at=clock_timestamp() WHERE id=$1")
                    .bind(f.sandbox.uuid())
                    .execute(&pool)
                    .await
                    .unwrap();
            }
            "rotate" => {
                let other = ProjectToken::generate().unwrap();
                sqlx::query("UPDATE projects SET api_tokens=jsonb_set(api_tokens,'{0,hash}',$2) WHERE id=$1").bind(f.project.uuid()).bind(json!(hex::encode(other.hash().as_bytes()))).execute(&pool).await.unwrap();
            }
            "suspend" => {
                sqlx::query("UPDATE projects SET status='suspended' WHERE id=$1")
                    .bind(f.project.uuid())
                    .execute(&pool)
                    .await
                    .unwrap();
            }
            "epoch" => {
                sqlx::query("UPDATE hosts SET supervisor_epoch=2 WHERE id=$1")
                    .bind(f.host.uuid())
                    .execute(&pool)
                    .await
                    .unwrap();
            }
            "destroy" => {
                sqlx::query("UPDATE sandboxes SET desired_state='destroyed' WHERE id=$1")
                    .bind(f.sandbox.uuid())
                    .execute(&pool)
                    .await
                    .unwrap();
            }
            _ => {
                sqlx::query("UPDATE allocations SET lease_expires_at=clock_timestamp()-interval '1 second' WHERE id=$1").bind(f.allocation.uuid()).execute(&pool).await.unwrap();
            }
        }
        if change == "error-revoke" {
            reader.fail.store(true, Ordering::SeqCst);
        }
        reader.go.add_permits(1);
        let response = task.await.unwrap();
        let auth = matches!(
            change,
            "revoke" | "rotate" | "suspend" | "error-revoke" | "token-expiry"
        );
        problem(
            &response,
            if auth {
                StatusCode::UNAUTHORIZED
            } else {
                StatusCode::NOT_FOUND
            },
            if auth { "unauthenticated" } else { "not_found" },
        );
    }
}
#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn lock_waits_cannot_return_an_old_credential_or_allocation_snapshot(pool: PgPool) {
    let _s = SERIAL.lock().await;
    for project_lock in [true, false] {
        let (f, reader, app) = setup(&pool, b"private bytes".to_vec()).await;
        let token = capture(&app, &f).await;
        reader.started.notified().await;
        reader.pause.store(true, Ordering::SeqCst);
        let a = app.clone();
        let bearer = f.token.clone();
        let url = route(&f);
        let task =
            tokio::spawn(
                async move { call(a, &bearer, "GET", &url, Some(&token), Value::Null).await },
            );
        tokio::time::timeout(Duration::from_secs(3), reader.started.notified())
            .await
            .unwrap();
        let mut tx = pool.begin().await.unwrap();
        let query = if project_lock {
            "SELECT id FROM projects WHERE id=$1 FOR UPDATE"
        } else {
            "SELECT id FROM sandboxes WHERE id=$1 FOR UPDATE"
        };
        sqlx::query(query)
            .bind(if project_lock {
                f.project.uuid()
            } else {
                f.sandbox.uuid()
            })
            .fetch_one(&mut *tx)
            .await
            .unwrap();
        reader.go.add_permits(1);
        let pattern = if project_lock {
            "SELECT id FROM projects WHERE id=$1 FOR SHARE"
        } else {
            "SELECT current_allocation_id FROM sandboxes WHERE id=$1 AND project_id=$2 FOR SHARE"
        };
        tokio::time::timeout(Duration::from_secs(3),async {loop {
            let waiting:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE datname=current_database() AND wait_event_type='Lock' AND query=$1)").bind(pattern).fetch_one(&pool).await.unwrap();
            if waiting{break;}tokio::time::sleep(Duration::from_millis(10)).await;
        }}).await.unwrap();
        if project_lock {
            sqlx::query("UPDATE projects SET api_tokens=jsonb_set(api_tokens,'{0,revoked_at}',to_jsonb(clock_timestamp())) WHERE id=$1").bind(f.project.uuid()).execute(&mut *tx).await.unwrap();
        } else {
            sqlx::query("UPDATE sandboxes SET desired_state='destroyed' WHERE id=$1")
                .bind(f.sandbox.uuid())
                .execute(&mut *tx)
                .await
                .unwrap();
        }
        tx.commit().await.unwrap();
        let response = task.await.unwrap();
        problem(
            &response,
            if project_lock {
                StatusCode::UNAUTHORIZED
            } else {
                StatusCode::NOT_FOUND
            },
            if project_lock {
                "unauthenticated"
            } else {
                "not_found"
            },
        );
    }
}
#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn capture_release_and_capacity_use_the_same_final_authorization_gate(pool: PgPool) {
    let _s = SERIAL.lock().await;
    for method in ["POST", "DELETE"] {
        let (f, reader, app) = setup(&pool, b"data".to_vec()).await;
        let token = capture(&app, &f).await;
        reader.started.notified().await;
        reader.pause.store(true, Ordering::SeqCst);
        let a = app.clone();
        let bearer = f.token.clone();
        let url = route(&f);
        let task = tokio::spawn(async move {
            call(
                a,
                &bearer,
                method,
                &url,
                if method == "DELETE" {
                    Some(token.as_str())
                } else {
                    None
                },
                if method == "POST" {
                    json!({"path":"file.bin"})
                } else {
                    Value::Null
                },
            )
            .await
        });
        tokio::time::timeout(Duration::from_secs(3), reader.started.notified())
            .await
            .unwrap();
        sqlx::query("UPDATE projects SET status='suspended' WHERE id=$1")
            .bind(f.project.uuid())
            .execute(&pool)
            .await
            .unwrap();
        reader.go.add_permits(1);
        problem(
            &task.await.unwrap(),
            StatusCode::UNAUTHORIZED,
            "unauthenticated",
        );
    }
    let (f, reader, app) = setup(&pool, b"data".to_vec()).await;
    let token = capture(&app, &f).await;
    reader.pause.store(true, Ordering::SeqCst);
    let before = reader.calls.load(Ordering::SeqCst);
    let mut tasks = Vec::new();
    for _ in 0..4 {
        let a = app.clone();
        let t = token.clone();
        let b = f.token.clone();
        let url = route(&f);
        tasks.push(tokio::spawn(async move {
            call(a, &b, "GET", &url, Some(&t), Value::Null).await
        }));
    }
    tokio::time::timeout(Duration::from_secs(3), async {
        while reader.calls.load(Ordering::SeqCst) < before + 4 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    problem(
        &call(
            app.clone(),
            &f.token,
            "GET",
            &route(&f),
            Some(&token),
            Value::Null,
        )
        .await,
        StatusCode::SERVICE_UNAVAILABLE,
        "unavailable",
    );
    reader.pause.store(false, Ordering::SeqCst);
    reader.go.add_permits(4);
    for task in tasks {
        assert_eq!(task.await.unwrap().0, StatusCode::OK);
    }
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn capture_provenance_and_expired_handles_fail_closed(pool: PgPool) {
    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD as B64};
    let _s = SERIAL.lock().await;
    let (f, reader, app) = setup(&pool, b"data".to_vec()).await;
    let token = capture(&app, &f).await;
    let mut expired: Value = serde_json::from_slice(&B64.decode(&token).unwrap()).unwrap();
    expired["handle"]["expires_unix_ms"] = (now() - 1).into();
    expired["handle"]["capture"]["expires_unix_ms"] = (now() - 1).into();
    let expired = B64.encode(serde_json::to_vec(&expired).unwrap());
    let before = reader.calls.load(Ordering::SeqCst);
    problem(
        &call(
            app.clone(),
            &f.token,
            "GET",
            &route(&f),
            Some(&expired),
            Value::Null,
        )
        .await,
        StatusCode::GONE,
        "file_capture_missing",
    );
    assert_eq!(reader.calls.load(Ordering::SeqCst), before);
    reader.corrupt.store(true, Ordering::SeqCst);
    problem(
        &call(
            app,
            &f.token,
            "POST",
            &route(&f),
            None,
            json!({"path":"file.bin"}),
        )
        .await,
        StatusCode::BAD_GATEWAY,
        "file_response_invalid",
    );
}
