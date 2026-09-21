//! SSE consumer tests with real SQL authorization and controlled guest/storage reads.
#![allow(clippy::unwrap_used, clippy::expect_used)]
#[path = "support/output.rs"]
mod fixture;
use axum::{
    Router,
    body::{Body, BodyDataStream, to_bytes},
    response::Response,
};
use base64::{
    Engine,
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
};
use fixture::Fixture;
use http::{Request, StatusCode};
use sandbox_api::{outputs::OutputReader, problem::Problem, streams::live::LiveReader};
use sandbox_artifacts::{Error, OutputChunk};
use sandbox_protocol::{
    HostId, Id, guest as w, guest_model as m,
    live_output::LiveOutputScope,
    output::{OutputName, OutputOwner, OutputRef},
    supervisor::{LiveOutputObservation, LiveOutputRequest},
};
use serde_json::Value;
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
use tokio::sync::{Notify, Semaphore};
use tokio_stream::StreamExt;
use tower::ServiceExt;

#[derive(Debug)]
struct Live {
    mode: AtomicUsize,
    calls: AtomicUsize,
    started: Notify,
    release: Semaphore,
}
impl Live {
    fn new(mode: usize) -> Self {
        Self {
            mode: AtomicUsize::new(mode),
            calls: AtomicUsize::new(0),
            started: Notify::new(),
            release: Semaphore::new(0),
        }
    }
}
impl LiveReader for Live {
    fn read<'a>(
        &'a self,
        host: HostId,
        request: LiveOutputRequest,
    ) -> Pin<Box<dyn Future<Output = Result<LiveOutputObservation, Problem>> + Send + 'a>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.started.notify_one();
            if self.mode.load(Ordering::SeqCst) == 2 {
                self.release.acquire().await.unwrap().forget();
            }
            let mode = self.mode.load(Ordering::SeqCst);
            let scope: LiveOutputScope = serde_json::from_slice(&request.scope_json).unwrap();
            assert_eq!(host, scope.owner.host_id);
            let complete = mode == 1;
            let out = if complete {
                b"\0\xffaz".as_slice()
            } else {
                b"\0\xffa".as_slice()
            };
            let err = b"\xfee".as_slice();
            let r = request.output.as_ref().unwrap();
            let bytes = if r.stream == w::Stream::Stdout as i32 {
                out
            } else {
                err
            };
            if r.offset > bytes.len() as u64 {
                return Err(Problem::OutputRange);
            }
            let next = bytes.len().min(r.offset as usize + r.limit as usize);
            let chunk = w::OutputChunk {
                operation_id: r.operation_id.clone(),
                stream: r.stream,
                offset: r.offset,
                data: bytes[r.offset as usize..next].to_vec(),
                next_offset: next as u64,
                at_end: next == bytes.len(),
                complete,
            };
            let receipt = m::Receipt {
                version: 1,
                context: m::Context {
                    allocation_id: scope.owner.allocation_id,
                    generation: scope.owner.generation,
                    boot_id: scope.owner.boot_id,
                },
                operation_id: scope.owner.operation_id,
                digest: scope.command_digest,
                state: if complete {
                    m::State::Exited
                } else {
                    m::State::LaunchIntent
                },
                deadline_unix_ms: scope.deadline_unix_ms,
                output_limit: scope.output_limit,
                cancel_requested: false,
                cleanup_confirmed: complete,
                exit_code: if complete { Some(0) } else { None },
                signal: None,
                stdout: m::Output {
                    seen: out.len() as u64,
                    stored: out.len() as u64,
                    truncated: false,
                },
                stderr: m::Output {
                    seen: 2,
                    stored: 2,
                    truncated: false,
                },
                reason: None,
            };
            let mut reply = LiveOutputObservation {
                request: Some(request),
                host_id: host.to_string(),
                supervisor_epoch: scope.owner.host_epoch,
                simulated: true,
                observed_unix_ms: (time::OffsetDateTime::now_utc().unix_timestamp_nanos()
                    / 1_000_000) as i64,
                chunk: Some(chunk),
                receipt: Some((&receipt).into()),
            };
            if mode == 3 {
                reply.host_id = HostId::generate().to_string();
            }
            Ok(reply)
        })
    }
}
#[derive(Debug)]
struct Archive;
impl OutputReader for Archive {
    fn read<'a>(
        &'a self,
        r: &'a OutputRef,
        _: &'a OutputOwner,
        _: i64,
        offset: u64,
        limit: usize,
    ) -> Pin<Box<dyn Future<Output = Result<OutputChunk, Error>> + Send + 'a>> {
        Box::pin(async move {
            let bytes = match r.plan.name {
                OutputName::Stdout => b"a\0b\xff".as_slice(),
                OutputName::Stderr => b"err".as_slice(),
            };
            if offset > bytes.len() as u64 {
                return Err(Error::Bounds);
            }
            let next = bytes.len().min(offset as usize + limit);
            Ok(OutputChunk {
                bytes: bytes[offset as usize..next].to_vec(),
                next_offset: next as u64,
                eof: next == bytes.len(),
                truncated: r.plan.truncated,
            })
        })
    }
}
fn app(f: &Fixture, live: Option<Arc<Live>>) -> Router {
    sandbox_api::router_with_streams(
        sandbox_api::AppState {
            store: f.store.clone(),
            images: sandbox_protocol::images::ImageAllowlist::new([format!(
                "sha256:{}",
                "a".repeat(64)
            )])
            .unwrap(),
        },
        Some(Arc::new(Archive)),
        live.map(|v| v as Arc<dyn LiveReader>),
    )
}
async fn open(app: Router, f: &Fixture, cursor: Option<&str>) -> Response {
    let mut req = Request::builder()
        .uri(format!("/v1/operations/{}/stream", f.operation))
        .header("authorization", format!("Bearer {}", f.token));
    if let Some(cursor) = cursor {
        req = req.header("last-event-id", cursor);
    }
    app.oneshot(req.body(Body::empty()).unwrap()).await.unwrap()
}
async fn frame(body: &mut BodyDataStream) -> String {
    let bytes = tokio::time::timeout(Duration::from_secs(3), body.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    String::from_utf8(bytes.to_vec()).unwrap()
}
fn data(frame: &str) -> Value {
    serde_json::from_str(
        frame
            .lines()
            .find_map(|l| l.strip_prefix("data: ").or_else(|| l.strip_prefix("data:")))
            .unwrap(),
    )
    .unwrap()
}
fn cursor(frame: &str) -> String {
    frame
        .lines()
        .find_map(|l| l.strip_prefix("id: ").or_else(|| l.strip_prefix("id:")))
        .unwrap()
        .to_string()
}
async fn active(pool: &PgPool) -> Fixture {
    let mut f = Fixture::new(pool).await;
    let r = f.observation.receipt.as_mut().unwrap();
    r.state = w::State::LaunchIntent as i32;
    r.exit_code = None;
    r.cleanup_confirmed = false;
    f.finish().await;
    f
}
async fn revoke(f: &Fixture) {
    sqlx::query("UPDATE projects SET api_tokens='[]' WHERE id=$1")
        .bind(f.project.uuid())
        .execute(f.store.pool())
        .await
        .unwrap();
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn binary_live_reconnect_finishes_without_reexecution_or_mutating_evidence(pool: PgPool) {
    let f = active(&pool).await;
    let before = f.snapshot().await;
    let live = Arc::new(Live::new(0));
    let app = app(&f, Some(live.clone()));
    let response = open(app.clone(), &f, None).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["content-type"], "text/event-stream");
    assert_eq!(response.headers()["cache-control"], "no-store");
    let mut body = response.into_body().into_data_stream();
    let first = frame(&mut body).await;
    assert_eq!(
        STANDARD
            .decode(data(&first)["data_base64"].as_str().unwrap())
            .unwrap(),
        [0, 255, b'a']
    );
    assert_eq!(data(&first)["complete"], false);
    let id = cursor(&first);
    drop(body);
    tokio::task::yield_now().await;
    let decoded: String = String::from_utf8(URL_SAFE_NO_PAD.decode(&id).unwrap()).unwrap();
    assert!(!decoded.contains(&f.host.to_string()));
    live.mode.store(1, Ordering::SeqCst);
    let response = open(app, &f, Some(&id)).await;
    assert_eq!(response.status(), StatusCode::OK);
    let mut body = response.into_body().into_data_stream();
    let tail = frame(&mut body).await;
    assert_eq!(data(&tail)["offset"], 3);
    assert_eq!(
        STANDARD
            .decode(data(&tail)["data_base64"].as_str().unwrap())
            .unwrap(),
        b"z"
    );
    let stderr = frame(&mut body).await;
    assert_eq!(data(&stderr)["stream"], "stderr");
    assert_eq!(
        STANDARD
            .decode(data(&stderr)["data_base64"].as_str().unwrap())
            .unwrap(),
        [254, b'e']
    );
    let end = frame(&mut body).await;
    assert!(end.contains("event: end"));
    assert_eq!(data(&end)["reason"], "complete");
    assert!(body.next().await.is_none());
    assert_eq!(before, f.snapshot().await);
}
#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn missing_foreign_pending_expired_and_malformed_streams_are_explicit(pool: PgPool) {
    let f = Fixture::new(&pool).await;
    let live = Arc::new(Live::new(0));
    let app = app(&f, Some(live));
    assert_eq!(
        open(app.clone(), &f, None).await.status(),
        StatusCode::CONFLICT
    );
    f.finish().await;
    assert_eq!(
        open(app.clone(), &f, Some("invalid")).await.status(),
        StatusCode::BAD_REQUEST
    );
    let foreign = Fixture::new(&pool).await;
    let req = Request::builder()
        .uri(format!("/v1/operations/{}/stream", f.operation))
        .header("authorization", format!("Bearer {}", foreign.token))
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        app.clone().oneshot(req).await.unwrap().status(),
        StatusCode::NOT_FOUND
    );
    for query in ["?unknown=1", "?cursor=x&cursor=y"] {
        let req = Request::builder()
            .uri(format!("/v1/operations/{}/stream{query}", f.operation))
            .header("authorization", format!("Bearer {}", f.token))
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            app.clone().oneshot(req).await.unwrap().status(),
            StatusCode::BAD_REQUEST
        );
    }
    sqlx::query("UPDATE operations SET response_expires_at=clock_timestamp()-interval '1 second' WHERE id=$1").bind(f.operation.uuid()).execute(&pool).await.unwrap();
    assert_eq!(open(app, &f, None).await.status(), StatusCode::GONE);
}
#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn revocation_during_live_read_releases_no_bytes(pool: PgPool) {
    let f = active(&pool).await;
    let live = Arc::new(Live::new(2));
    let app = app(&f, Some(live.clone()));
    let response = open(app, &f, None).await;
    assert_eq!(response.status(), StatusCode::OK);
    live.started.notified().await;
    revoke(&f).await;
    live.release.add_permits(1);
    let bytes = tokio::time::timeout(
        Duration::from_secs(3),
        to_bytes(response.into_body(), 100000),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(bytes.is_empty());
}
#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn idle_unpolled_consumer_still_rechecks_revocation_and_discards_queued_bytes(pool: PgPool) {
    let f = active(&pool).await;
    let live = Arc::new(Live::new(0));
    let response = open(app(&f, Some(live.clone())), &f, None).await;
    live.started.notified().await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        live.calls.load(Ordering::SeqCst) <= 2,
        "backpressure did not bound reads"
    );
    revoke(&f).await;
    tokio::time::sleep(Duration::from_secs(6)).await;
    let bytes = tokio::time::timeout(
        Duration::from_secs(1),
        to_bytes(response.into_body(), 100000),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(bytes.is_empty());
}
#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn stream_capacity_returns_after_disconnect_and_forged_reply_is_not_delivered(pool: PgPool) {
    let f = active(&pool).await;
    let live = Arc::new(Live::new(0));
    let app = app(&f, Some(live.clone()));
    let mut held = Vec::new();
    for _ in 0..4 {
        let r = open(app.clone(), &f, None).await;
        assert_eq!(r.status(), StatusCode::OK);
        held.push(r);
    }
    assert_eq!(
        open(app.clone(), &f, None).await.status(),
        StatusCode::SERVICE_UNAVAILABLE
    );
    held.pop();
    live.mode.store(3, Ordering::SeqCst);
    let response = open(app, &f, None).await;
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = to_bytes(response.into_body(), 100000).await.unwrap();
    let text = String::from_utf8(bytes.to_vec()).unwrap();
    assert!(text.contains("output_corrupt"));
    assert!(!text.contains("data_base64"));
}
#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn publication_during_live_read_switches_to_verified_archived_bytes(pool: PgPool) {
    let f = Fixture::new(&pool).await;
    f.finish().await;
    let live = Arc::new(Live::new(2));
    let response = open(app(&f, Some(live.clone())), &f, None).await;
    live.started.notified().await;
    let (claim, work) = f.work().await;
    let plans = fixture::plans(&work);
    f.store
        .save_output_plans(&claim, &plans, true)
        .await
        .unwrap();
    f.store
        .publish_output(&claim, &fixture::refs(&plans), true)
        .await
        .unwrap();
    live.release.add_permits(1);
    let mut body = response.into_body().into_data_stream();
    let stdout = frame(&mut body).await;
    assert_eq!(
        STANDARD
            .decode(data(&stdout)["data_base64"].as_str().unwrap())
            .unwrap(),
        b"a\0b\xff"
    );
    assert_eq!(data(&stdout)["truncated"], true);
    let id = cursor(&stdout);
    drop(body);
    sqlx::query("UPDATE hosts SET supervisor_epoch=2 WHERE id=$1")
        .bind(f.host.uuid())
        .execute(&pool)
        .await
        .unwrap();
    let response = open(app(&f, None), &f, Some(&id)).await;
    let bytes = to_bytes(response.into_body(), 100000).await.unwrap();
    let text = String::from_utf8(bytes.to_vec()).unwrap();
    assert!(text.contains("event: end"));
    assert!(text.contains("ZXJy"));
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn cursor_cannot_change_tenant_operation_binding_or_exceed_reserved_bound(pool: PgPool) {
    let f = active(&pool).await;
    let live = Arc::new(Live::new(0));
    let app = app(&f, Some(live));
    let mut body = open(app.clone(), &f, None)
        .await
        .into_body()
        .into_data_stream();
    let id = cursor(&frame(&mut body).await);
    drop(body);
    let parsed: Value = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(&id).unwrap()).unwrap();
    for (field, value, status) in [
        (
            "project",
            serde_json::json!(sandbox_protocol::ProjectId::generate()),
            StatusCode::BAD_REQUEST,
        ),
        (
            "operation",
            serde_json::json!(sandbox_protocol::OperationId::generate()),
            StatusCode::BAD_REQUEST,
        ),
        (
            "offsets",
            serde_json::json!([u64::MAX, 1]),
            StatusCode::BAD_REQUEST,
        ),
        ("version", serde_json::json!(2), StatusCode::BAD_REQUEST),
        ("binding", serde_json::json!("different"), StatusCode::GONE),
    ] {
        let mut value_in = parsed.clone();
        value_in[field] = value;
        let encoded = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&value_in).unwrap());
        assert_eq!(
            open(app.clone(), &f, Some(&encoded)).await.status(),
            status,
            "{field}"
        );
    }
    let req = Request::builder()
        .uri(format!("/v1/operations/{}/stream?cursor={id}", f.operation))
        .header("authorization", format!("Bearer {}", f.token))
        .header("last-event-id", &id)
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        app.oneshot(req).await.unwrap().status(),
        StatusCode::BAD_REQUEST
    );
}
#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn revocation_while_final_metadata_lookup_waits_does_not_leak_bytes(pool: PgPool) {
    let f = active(&pool).await;
    let live = Arc::new(Live::new(2));
    let response = open(app(&f, Some(live.clone())), &f, None).await;
    live.started.notified().await;
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("LOCK TABLE operations IN ACCESS EXCLUSIVE MODE")
        .execute(&mut *tx)
        .await
        .unwrap();
    live.release.add_permits(1);
    let until = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        let waiting:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE datname=current_database() AND wait_event_type='Lock')").fetch_one(&pool).await.unwrap();
        if waiting {
            break;
        }
        assert!(tokio::time::Instant::now() < until);
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    revoke(&f).await;
    tx.commit().await.unwrap();
    let bytes = tokio::time::timeout(
        Duration::from_secs(2),
        to_bytes(response.into_body(), 100000),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(bytes.is_empty());
}
#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn expiry_after_slow_read_is_gap_not_empty_success(pool: PgPool) {
    let f = active(&pool).await;
    let live = Arc::new(Live::new(2));
    let before = f.snapshot().await;
    let response = open(app(&f, Some(live.clone())), &f, None).await;
    live.started.notified().await;
    sqlx::query("UPDATE operations SET response_expires_at=clock_timestamp()-interval '1 second' WHERE id=$1").bind(f.operation.uuid()).execute(&pool).await.unwrap();
    live.release.add_permits(1);
    let bytes = to_bytes(response.into_body(), 100000).await.unwrap();
    let text = std::str::from_utf8(&bytes).unwrap();
    assert!(text.contains("event: gap") && text.contains("output_expired"));
    assert!(!text.contains("data_base64"));
    assert_eq!(before, f.snapshot().await);
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn queued_bytes_are_discarded_when_retention_expires_before_consumer_polls(pool: PgPool) {
    let f = active(&pool).await;
    sqlx::query("UPDATE operations SET response_expires_at=clock_timestamp()+interval '400 milliseconds' WHERE id=$1").bind(f.operation.uuid()).execute(&pool).await.unwrap();
    let live = Arc::new(Live::new(0));
    let response = open(app(&f, Some(live.clone())), &f, None).await;
    assert_eq!(response.status(), StatusCode::OK);
    live.started.notified().await;
    tokio::time::sleep(Duration::from_millis(600)).await;
    let bytes = to_bytes(response.into_body(), 100000).await.unwrap();
    let text = std::str::from_utf8(&bytes).unwrap();
    assert!(
        !text.contains("data_base64"),
        "retention-expired queued output was delivered"
    );
    assert!(text.contains("output_expired"));
}
