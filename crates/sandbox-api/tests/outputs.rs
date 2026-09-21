//! Synthetic receipts plus real PostgreSQL/router; storage faults are controlled.
#![allow(clippy::unwrap_used, clippy::expect_used)]
#[path = "support/output.rs"]
mod fixture;
use axum::{
    Router,
    body::{Body, to_bytes},
};
use fixture::Fixture;
use http::{HeaderMap, Request, StatusCode};
use sandbox_api::outputs::OutputReader;
use sandbox_artifacts::{Error, OutputChunk};
use sandbox_protocol::{
    Id, OperationId,
    output::{OutputName, OutputOwner, OutputRef},
};
use sqlx::PgPool;
use std::{
    future::Future,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::{Mutex, Notify, Semaphore};
static SERIAL: Mutex<()> = Mutex::const_new(());
use tower::ServiceExt;

#[derive(Debug)]
struct Reader {
    calls: AtomicUsize,
    started: Notify,
    released: Semaphore,
    pause: bool,
    error: Option<Error>,
    bad_chunk: bool,
}
impl Default for Reader {
    fn default() -> Self {
        Self {
            calls: AtomicUsize::new(0),
            started: Notify::new(),
            released: Semaphore::new(0),
            pause: false,
            error: None,
            bad_chunk: false,
        }
    }
}
impl OutputReader for Reader {
    fn read<'a>(
        &'a self,
        r: &'a OutputRef,
        _: &'a OutputOwner,
        _: i64,
        offset: u64,
        limit: usize,
    ) -> Pin<Box<dyn Future<Output = Result<OutputChunk, Error>> + Send + 'a>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.started.notify_one();
            if self.pause {
                self.released.acquire().await.unwrap().forget();
            }
            if let Some(error) = self.error {
                return Err(error);
            }
            let bytes = match r.plan.name {
                OutputName::Stdout => b"a\x00b\xff".as_slice(),
                OutputName::Stderr => b"err".as_slice(),
            };
            let end = bytes.len().min(offset as usize + limit);
            Ok(OutputChunk {
                bytes: bytes[offset as usize..end].to_vec(),
                next_offset: end as u64 + u64::from(self.bad_chunk),
                eof: end == bytes.len(),
                truncated: r.plan.truncated,
            })
        })
    }
}
async fn get(app: Router, token: &str, path: &str) -> (StatusCode, HeaderMap, Vec<u8>) {
    let r = Request::builder()
        .uri(path)
        .header("authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(r).await.unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = to_bytes(response.into_body(), 65536)
        .await
        .unwrap()
        .to_vec();
    (status, headers, bytes)
}
fn path(f: &Fixture, name: &str) -> String {
    format!("/v1/operations/{}/outputs/{name}", f.operation)
}
fn problem(r: &(StatusCode, HeaderMap, Vec<u8>), status: StatusCode, code: &str) {
    assert_eq!(r.0, status, "{}", String::from_utf8_lossy(&r.2));
    assert_eq!(r.1["cache-control"], "no-store");
    assert_eq!(r.1["content-type"], "application/problem+json");
    let body: serde_json::Value = serde_json::from_slice(&r.2).unwrap();
    assert_eq!(body["code"], code);
    assert!(!r.2.windows(4).any(|v| v == b"a\x00b\xff"));
}
#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn binary_ranges_stderr_eof_and_truncation_preserve_execution(pool: PgPool) {
    let _serial = SERIAL.lock().await;
    let f = Fixture::new(&pool).await;
    f.publish().await;
    let before = f.snapshot().await;
    let reader = Arc::new(Reader::default());
    let app = f.app(Some(reader.clone()));
    let r = get(app.clone(), &f.token, &path(&f, "stdout?limit=2")).await;
    assert_eq!(r.0, StatusCode::OK);
    assert_eq!(r.2, b"a\x00");
    for (key, value) in [
        ("content-type", "application/octet-stream"),
        ("cache-control", "no-store"),
        ("x-content-type-options", "nosniff"),
        ("x-output-offset", "0"),
        ("x-output-next-offset", "2"),
        ("x-output-size", "4"),
        ("x-output-seen", "10"),
        ("x-output-eof", "false"),
        ("x-output-truncated", "true"),
        ("x-output-simulated", "true"),
    ] {
        assert_eq!(r.1[key], value);
    }
    let r = get(app.clone(), &f.token, &path(&f, "stdout?offset=2")).await;
    assert_eq!(r.2, b"b\xff");
    assert_eq!(r.1["x-output-eof"], "true");
    let r = get(app.clone(), &f.token, &path(&f, "stdout?offset=4")).await;
    assert_eq!(r.0, StatusCode::OK);
    assert!(r.2.is_empty());
    assert_eq!(r.1["x-output-eof"], "true");
    let r = get(app, &f.token, &path(&f, "stderr")).await;
    assert_eq!(r.2, b"err");
    assert_eq!(r.1["x-output-truncated"], "false");
    assert_eq!(f.snapshot().await, before);
    assert_eq!(reader.calls.load(Ordering::SeqCst), 4);
}
#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn reads_reject_other_tenants_bad_paths_queries_and_unconfigured_storage(pool: PgPool) {
    let _serial = SERIAL.lock().await;
    let f = Fixture::new(&pool).await;
    f.publish().await;
    let other = Fixture::new(&pool).await;
    let reader = Arc::new(Reader::default());
    let app = f.app(Some(reader.clone()));
    let denied = get(app.clone(), &other.token, &path(&f, "stdout")).await;
    problem(&denied, StatusCode::NOT_FOUND, "not_found");
    let missing = get(
        app.clone(),
        &other.token,
        &format!("/v1/operations/{}/outputs/stdout", OperationId::generate()),
    )
    .await;
    assert_eq!(denied.2, missing.2);
    problem(
        &get(app.clone(), "bad-token", &path(&f, "stdout")).await,
        StatusCode::UNAUTHORIZED,
        "unauthenticated",
    );
    problem(
        &get(app.clone(), &f.token, &path(&f, "customer-key")).await,
        StatusCode::NOT_FOUND,
        "not_found",
    );
    for query in [
        "limit=0",
        "limit=32769",
        "limit=1&limit=2",
        "offset=-1",
        "offset=18446744073709551616",
        "offset=1&offset=2",
        "bucket=other",
        "key=private",
        "url=http%3A%2F%2Fexample.test",
    ] {
        problem(
            &get(app.clone(), &f.token, &path(&f, &format!("stdout?{query}"))).await,
            StatusCode::BAD_REQUEST,
            "bad_request",
        );
    }
    problem(
        &get(app.clone(), &f.token, &path(&f, "stdout?offset=5")).await,
        StatusCode::RANGE_NOT_SATISFIABLE,
        "output_range_invalid",
    );
    problem(
        &get(f.app(None), &f.token, &path(&f, "stdout")).await,
        StatusCode::SERVICE_UNAVAILABLE,
        "unavailable",
    );
    problem(
        &get(
            app.clone(),
            &f.token,
            "/v1/operations/not-an-id/outputs/stdout",
        )
        .await,
        StatusCode::BAD_REQUEST,
        "bad_request",
    );
    let request = Request::builder()
        .uri(path(&f, "stdout"))
        .header("authorization", format!("Bearer {}", f.token))
        .header("range", "bytes=0-1")
        .body(Body::empty())
        .unwrap();
    let rejected = app.clone().oneshot(request).await.unwrap();
    assert_eq!(rejected.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        rejected.headers()["content-type"],
        "application/problem+json"
    );
    let create=Request::builder().method("POST").uri("/v1/sandboxes").header("authorization",format!("Bearer {}",f.token)).header("content-type","application/json").header("idempotency-key",OperationId::generate().to_string()).body(Body::from(serde_json::json!({"image_digest":format!("sha256:{}","a".repeat(64)),"resources":{"vcpu":1,"memory_mib":128,"disk_mib":64}}).to_string())).unwrap();
    let created = app.clone().oneshot(create).await.unwrap();
    assert_eq!(created.status(), StatusCode::ACCEPTED);
    let body: serde_json::Value =
        serde_json::from_slice(&to_bytes(created.into_body(), 65536).await.unwrap()).unwrap();
    problem(
        &get(
            app,
            &f.token,
            &format!(
                "/v1/operations/{}/outputs/stdout",
                body["operation_id"].as_str().unwrap()
            ),
        )
        .await,
        StatusCode::NOT_FOUND,
        "not_found",
    );
    assert_eq!(reader.calls.load(Ordering::SeqCst), 0);
}
#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn readiness_expiry_and_corrupt_reference_never_invent_empty_output(pool: PgPool) {
    let _serial = SERIAL.lock().await;
    let f = Fixture::new(&pool).await;
    let reader = Arc::new(Reader::default());
    let app = f.app(Some(reader.clone()));
    problem(
        &get(app.clone(), &f.token, &path(&f, "stdout")).await,
        StatusCode::CONFLICT,
        "output_not_ready",
    );
    sqlx::query("UPDATE operations SET response_expires_at=clock_timestamp()-interval '1 second' WHERE id=$1").bind(f.operation.uuid()).execute(&pool).await.unwrap();
    problem(
        &get(app.clone(), &f.token, &path(&f, "stdout")).await,
        StatusCode::GONE,
        "output_expired",
    );
    sqlx::query("UPDATE operations SET response_expires_at=NULL WHERE id=$1")
        .bind(f.operation.uuid())
        .execute(&pool)
        .await
        .unwrap();
    f.finish().await;
    problem(
        &get(app.clone(), &f.token, &path(&f, "stdout")).await,
        StatusCode::CONFLICT,
        "output_not_ready",
    );
    let (claim, work) = f.work().await;
    let plans = fixture::plans(&work);
    problem(
        &get(app.clone(), &f.token, &path(&f, "stdout")).await,
        StatusCode::CONFLICT,
        "output_not_ready",
    );
    f.store
        .save_output_plans(&claim, &plans, true)
        .await
        .unwrap();
    f.store
        .publish_output(&claim, &fixture::refs(&plans), true)
        .await
        .unwrap();
    sqlx::query("UPDATE operations SET output_refs=jsonb_set(output_refs,'{0,etag}','\"tampered\"') WHERE id=$1").bind(f.operation.uuid()).execute(&pool).await.unwrap();
    // A changed ETag is structurally valid; use an owner mismatch for persisted corruption.
    sqlx::query("UPDATE operations SET output_refs=jsonb_set(output_refs,'{0,plan,owner,generation}','99') WHERE id=$1").bind(f.operation.uuid()).execute(&pool).await.unwrap();
    problem(
        &get(app.clone(), &f.token, &path(&f, "stdout")).await,
        StatusCode::SERVICE_UNAVAILABLE,
        "unavailable",
    );
    sqlx::query("UPDATE operations SET output_refs=$2,response_expires_at=clock_timestamp()-interval '1 second' WHERE id=$1").bind(f.operation.uuid()).bind(serde_json::json!([fixture::refs(&plans).stdout,fixture::refs(&plans).stderr])).execute(&pool).await.unwrap();
    problem(
        &get(app, &f.token, &path(&f, "stdout")).await,
        StatusCode::GONE,
        "output_expired",
    );
    assert_eq!(reader.calls.load(Ordering::SeqCst), 0);
}
#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn storage_failures_and_invalid_chunks_remain_explicit(pool: PgPool) {
    let _serial = SERIAL.lock().await;
    let f = Fixture::new(&pool).await;
    f.publish().await;
    for (error, status, code) in [
        (Error::Missing, StatusCode::GONE, "output_missing"),
        (Error::Corrupt, StatusCode::BAD_GATEWAY, "output_corrupt"),
        (
            Error::Unavailable,
            StatusCode::SERVICE_UNAVAILABLE,
            "unavailable",
        ),
        (Error::Busy, StatusCode::SERVICE_UNAVAILABLE, "unavailable"),
        (Error::Expired, StatusCode::GONE, "output_expired"),
    ] {
        let app = f.app(Some(Arc::new(Reader {
            error: Some(error),
            ..Reader::default()
        })));
        problem(&get(app, &f.token, &path(&f, "stdout")).await, status, code);
    }
    problem(
        &get(
            f.app(Some(Arc::new(Reader {
                bad_chunk: true,
                ..Reader::default()
            }))),
            &f.token,
            &path(&f, "stdout"),
        )
        .await,
        StatusCode::BAD_GATEWAY,
        "output_corrupt",
    );
}
#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn access_and_retention_are_rechecked_after_slow_reads(pool: PgPool) {
    let _serial = SERIAL.lock().await;
    for change in [
        "revoke",
        "expire-token",
        "remove-token",
        "replace-secret",
        "suspend",
        "delete-project",
        "expire-output",
        "change-reference",
        "database-unavailable",
    ] {
        let f = Fixture::new(&pool).await;
        f.publish().await;
        let reader = Arc::new(Reader {
            pause: true,
            ..Reader::default()
        });
        let app = f.app(Some(reader.clone()));
        let token = f.token.clone();
        let url = path(&f, "stdout");
        let task = tokio::spawn(async move { get(app, &token, &url).await });
        tokio::time::timeout(Duration::from_secs(2), reader.started.notified())
            .await
            .unwrap();
        match change {
            "revoke" => {
                sqlx::query("UPDATE projects SET api_tokens=jsonb_set(api_tokens,'{0,revoked_at}',to_jsonb(clock_timestamp()::text)) WHERE id=$1").bind(f.project.uuid()).execute(&pool).await.unwrap();
            }
            "expire-token" => {
                sqlx::query("UPDATE projects SET api_tokens=jsonb_set(api_tokens,'{0,expires_at}',to_jsonb(clock_timestamp()::text)) WHERE id=$1").bind(f.project.uuid()).execute(&pool).await.unwrap();
            }
            "remove-token" => {
                sqlx::query("UPDATE projects SET api_tokens='[]' WHERE id=$1")
                    .bind(f.project.uuid())
                    .execute(&pool)
                    .await
                    .unwrap();
            }
            "replace-secret" => {
                sqlx::query("UPDATE projects SET api_tokens=jsonb_set(api_tokens,'{0,hash}',to_jsonb(repeat('a',64))) WHERE id=$1").bind(f.project.uuid()).execute(&pool).await.unwrap();
            }
            "suspend" | "delete-project" => {
                sqlx::query("UPDATE projects SET status=$2 WHERE id=$1")
                    .bind(f.project.uuid())
                    .bind(if change == "suspend" {
                        "suspended"
                    } else {
                        "deleting"
                    })
                    .execute(&pool)
                    .await
                    .unwrap();
            }
            "expire-output" => {
                sqlx::query("UPDATE operations SET response_expires_at=clock_timestamp()-interval '1 second' WHERE id=$1").bind(f.operation.uuid()).execute(&pool).await.unwrap();
            }
            "change-reference" => {
                sqlx::query("UPDATE operations SET output_refs=jsonb_set(output_refs,'{0,etag}','\"replacement\"') WHERE id=$1").bind(f.operation.uuid()).execute(&pool).await.unwrap();
            }
            _ => {
                f.store.pool().close().await;
            }
        }
        reader.released.add_permits(1);
        let r = task.await.unwrap();
        let (status, code) = match change {
            "expire-output" => (StatusCode::GONE, "output_expired"),
            "change-reference" | "database-unavailable" => {
                (StatusCode::SERVICE_UNAVAILABLE, "unavailable")
            }
            _ => (StatusCode::UNAUTHORIZED, "unauthenticated"),
        };
        problem(&r, status, code);
    }
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn read_slots_and_deadlines_bound_slow_storage(pool: PgPool) {
    let _serial = SERIAL.lock().await;
    let f = Fixture::new(&pool).await;
    f.publish().await;
    let reader = Arc::new(Reader {
        pause: true,
        ..Reader::default()
    });
    let app = f.app(Some(reader.clone()));
    let mut tasks = Vec::new();
    for _ in 0..4 {
        let app = app.clone();
        let token = f.token.clone();
        let url = path(&f, "stdout");
        tasks.push(tokio::spawn(async move { get(app, &token, &url).await }));
    }
    tokio::time::timeout(Duration::from_secs(3), async {
        while reader.calls.load(Ordering::SeqCst) < 4 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    problem(
        &get(app.clone(), &f.token, &path(&f, "stdout")).await,
        StatusCode::SERVICE_UNAVAILABLE,
        "unavailable",
    );
    problem(
        &get(app.clone(), "bad", &path(&f, "stdout")).await,
        StatusCode::UNAUTHORIZED,
        "unauthenticated",
    );
    assert_eq!(reader.calls.load(Ordering::SeqCst), 4);
    reader.released.add_permits(4);
    for task in tasks {
        assert_eq!(task.await.unwrap().0, StatusCode::OK);
    }
    let stalled = Arc::new(Reader {
        pause: true,
        ..Reader::default()
    });
    let app = f.app(Some(stalled.clone()));
    let token = f.token.clone();
    let url = path(&f, "stdout");
    let task = tokio::spawn(async move { get(app, &token, &url).await });
    tokio::time::timeout(Duration::from_secs(2), stalled.started.notified())
        .await
        .unwrap();
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(26)).await;
    tokio::time::resume();
    problem(
        &tokio::time::timeout(Duration::from_secs(3), task)
            .await
            .unwrap()
            .unwrap(),
        StatusCode::SERVICE_UNAVAILABLE,
        "unavailable",
    );
    assert_eq!(
        get(
            f.app(Some(Arc::new(Reader::default()))),
            &f.token,
            &path(&f, "stdout")
        )
        .await
        .0,
        StatusCode::OK
    );
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn revocation_during_final_metadata_lookup_withholds_bytes(pool: PgPool) {
    let _serial = SERIAL.lock().await;
    let f = Fixture::new(&pool).await;
    f.publish().await;
    let reader = Arc::new(Reader {
        pause: true,
        ..Reader::default()
    });
    let app = f.app(Some(reader.clone()));
    let token = f.token.clone();
    let url = path(&f, "stdout");
    let task = tokio::spawn(async move { get(app, &token, &url).await });
    tokio::time::timeout(Duration::from_secs(2), reader.started.notified())
        .await
        .unwrap();
    let mut blocker = pool.begin().await.unwrap();
    sqlx::query("LOCK TABLE operations IN ACCESS EXCLUSIVE MODE")
        .execute(&mut *blocker)
        .await
        .unwrap();
    reader.released.add_permits(1);
    tokio::time::timeout(Duration::from_secs(3),async {
        loop {
            let waiting:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_locks WHERE locktype='relation' AND relation='operations'::regclass AND database=(SELECT oid FROM pg_database WHERE datname=current_database()) AND NOT granted)").fetch_one(&pool).await.unwrap();
            if waiting {break;}
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }).await.unwrap();
    sqlx::query("UPDATE projects SET api_tokens='[]' WHERE id=$1")
        .bind(f.project.uuid())
        .execute(&pool)
        .await
        .unwrap();
    blocker.commit().await.unwrap();
    problem(
        &task.await.unwrap(),
        StatusCode::UNAUTHORIZED,
        "unauthenticated",
    );
}
