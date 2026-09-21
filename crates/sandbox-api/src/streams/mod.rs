//! Bounded SSE delivery. Reading/disconnecting never executes or cancels work.
mod cursor;
pub mod live;
use crate::{auth::Authenticated, outputs::OutputReader, problem::Problem, reads::no_store};
use axum::{
    Router,
    extract::{FromRef, Path, Query, State},
    response::{IntoResponse, Response, Sse, sse::Event},
    routing::get,
};
use base64::{Engine, engine::general_purpose::STANDARD};
use cursor::Cursor;
use http::{HeaderMap, header};
use sandbox_protocol::{
    OperationId,
    command::CommandRecord,
    guest as w, guest_model as m,
    output::{MAX_CHUNK, OutputName, OutputOwner},
    supervisor::LiveOutputRequest,
};
use sandbox_store::{
    Store,
    stream::{StreamSource, StreamView},
};
use serde::Deserialize;
use serde_json::json;
use std::{
    convert::Infallible,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicI64, Ordering},
    },
    task::{Context, Poll},
    time::Duration,
};
use time::OffsetDateTime;
use tokio::{
    sync::{OwnedSemaphorePermit, Semaphore, mpsc},
    task::JoinHandle,
    time::{sleep, timeout},
};
use tokio_stream::Stream;

const AUTH_INTERVAL: Duration = Duration::from_secs(5);
const META_TIMEOUT: Duration = Duration::from_secs(5);
const STREAM_LIFETIME: Duration = Duration::from_secs(90);
const SEND_TIMEOUT: Duration = Duration::from_secs(5);
const POLL_INTERVAL: Duration = Duration::from_millis(250);
#[derive(Debug, Clone)]
struct StreamState {
    store: Store,
    live: Option<Arc<dyn live::LiveReader>>,
    archive: Option<Arc<dyn OutputReader>>,
    slots: Arc<Semaphore>,
}
impl FromRef<StreamState> for Store {
    fn from_ref(s: &StreamState) -> Self {
        s.store.clone()
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Parameters {
    cursor: Option<String>,
}
fn now() -> i64 {
    (OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000) as i64
}
fn owner(view: &StreamView) -> Result<&OutputOwner, Problem> {
    if view
        .expires_at
        .is_some_and(|t| t <= OffsetDateTime::now_utc())
    {
        return Err(Problem::OutputExpired);
    }
    match &view.source {
        StreamSource::Live { scope, .. } => Ok(&scope.owner),
        StreamSource::Archived(v) => v.owner.as_ref().ok_or(Problem::Unavailable),
        StreamSource::Pending => Err(Problem::OutputNotReady),
        StreamSource::Missing => Err(Problem::OutputMissing),
        StreamSource::Expired => Err(Problem::OutputExpired),
    }
}
async fn view(
    state: &StreamState,
    caller: &Authenticated,
    op: OperationId,
) -> Result<StreamView, Problem> {
    timeout(
        META_TIMEOUT,
        state.store.stream_for_project(caller.project_id, op),
    )
    .await
    .map_err(|_| Problem::Unavailable)?
    .map_err(|_| Problem::Unavailable)?
    .ok_or(Problem::NotFound)
}
async fn authorize(state: &StreamState, caller: &Authenticated) -> Result<(), Problem> {
    timeout(META_TIMEOUT, caller.revalidate(&state.store))
        .await
        .map_err(|_| Problem::Unavailable)?
}
struct Chunk {
    bytes: Vec<u8>,
    offset: u64,
    next: u64,
    at_end: bool,
    complete: bool,
    stats: m::Output,
    simulated: bool,
    expires: i64,
}
async fn load(
    state: &StreamState,
    view: &StreamView,
    index: usize,
    offset: u64,
) -> Result<Chunk, Problem> {
    let name = if index == 0 {
        OutputName::Stdout
    } else {
        OutputName::Stderr
    };
    match &view.source {
        StreamSource::Archived(v) => {
            let refs = v.references.as_ref().ok_or(Problem::Unavailable)?;
            let reference = if index == 0 {
                &refs.stdout
            } else {
                &refs.stderr
            };
            let o = owner(view)?;
            if offset > reference.plan.size {
                return Err(Problem::OutputRange);
            }
            let reader = state.archive.as_ref().ok_or(Problem::Unavailable)?;
            let result = timeout(
                Duration::from_secs(25),
                reader.read(reference, o, now(), offset, MAX_CHUNK),
            )
            .await
            .map_err(|_| Problem::Unavailable)?
            .map_err(|e| match e {
                sandbox_artifacts::Error::Missing => Problem::OutputMissing,
                sandbox_artifacts::Error::Expired => Problem::OutputExpired,
                sandbox_artifacts::Error::Corrupt | sandbox_artifacts::Error::Conflict => {
                    Problem::OutputCorrupt
                }
                sandbox_artifacts::Error::Bounds => Problem::OutputRange,
                _ => Problem::Unavailable,
            })?;
            let expected = (reference.plan.size - offset).min(MAX_CHUNK as u64);
            if result.bytes.len() as u64 != expected
                || result.next_offset != offset + expected
                || result.eof != (result.next_offset == reference.plan.size)
                || result.truncated != reference.plan.truncated
            {
                return Err(Problem::OutputCorrupt);
            }
            Ok(Chunk {
                bytes: result.bytes,
                offset,
                next: result.next_offset,
                at_end: result.eof,
                complete: true,
                stats: m::Output {
                    stored: reference.plan.size,
                    seen: reference.plan.seen,
                    truncated: reference.plan.truncated,
                },
                simulated: v.simulated.ok_or(Problem::Unavailable)?,
                expires: reference.plan.expires_unix_ms,
            })
        }
        StreamSource::Live { scope, simulated } => {
            let reader = state.live.as_ref().ok_or(Problem::Unavailable)?;
            let request = LiveOutputRequest {
                scope_json: serde_json::to_vec(scope).map_err(|_| Problem::Internal)?,
                output: Some(w::ReadOutput {
                    operation_id: scope.owner.operation_id.to_string(),
                    stream: if name == OutputName::Stdout {
                        w::Stream::Stdout
                    } else {
                        w::Stream::Stderr
                    } as i32,
                    offset,
                    limit: MAX_CHUNK as u32,
                }),
                expires_unix_ms: now() + 15000,
            };
            let reply = timeout(
                Duration::from_secs(10),
                reader.read(scope.owner.host_id, request.clone()),
            )
            .await
            .map_err(|_| Problem::Unavailable)??;
            if reply.request.as_ref() != Some(&request)
                || reply.host_id != scope.owner.host_id.to_string()
                || reply.supervisor_epoch != scope.owner.host_epoch
                || reply.simulated != *simulated
                || reply.observed_unix_ms.abs_diff(now()) > 10000
            {
                return Err(Problem::OutputCorrupt);
            }
            let (_, read) = sandbox_supervisor::live_output::decode(&request, now())
                .map_err(|_| Problem::Unavailable)?;
            let receipt: m::Receipt = reply
                .receipt
                .ok_or(Problem::OutputCorrupt)?
                .try_into()
                .map_err(|_| Problem::OutputCorrupt)?;
            let chunk = reply.chunk.ok_or(Problem::OutputCorrupt)?;
            let command = CommandRecord {
                digest: scope.command_digest,
                context: Some(m::Context {
                    allocation_id: scope.owner.allocation_id,
                    generation: scope.owner.generation,
                    boot_id: scope.owner.boot_id.clone(),
                }),
                deadline_unix_ms: scope.deadline_unix_ms,
                output_limit: scope.output_limit,
                not_started: false,
                receipt: None,
            };
            sandbox_supervisor::live_output::validate_chunk(
                scope, &read, &command, &chunk, &receipt,
            )
            .map_err(|_| Problem::OutputCorrupt)?;
            Ok(Chunk {
                bytes: chunk.data,
                offset,
                next: chunk.next_offset,
                at_end: chunk.at_end,
                complete: chunk.complete,
                stats: if index == 0 {
                    receipt.stdout
                } else {
                    receipt.stderr
                },
                simulated: *simulated,
                expires: request.expires_unix_ms,
            })
        }
        _ => Err(Problem::OutputNotReady),
    }
}
fn same_source(a: &StreamView, b: &StreamView) -> bool {
    match (&a.source, &b.source) {
        (
            StreamSource::Live {
                scope: a,
                simulated: sa,
            },
            StreamSource::Live {
                scope: b,
                simulated: sb,
            },
        ) => a == b && sa == sb,
        (StreamSource::Archived(a), StreamSource::Archived(b)) => {
            a.owner == b.owner && a.references == b.references && a.simulated == b.simulated
        }
        _ => false,
    }
}
struct Pipe {
    tx: mpsc::Sender<Event>,
    denied: Arc<AtomicBool>,
    expires: Arc<AtomicI64>,
}
impl Pipe {
    fn retain_until(&self, at: Option<OffsetDateTime>) {
        if let Some(at) = at {
            self.expires.fetch_min(
                (at.unix_timestamp_nanos() / 1_000_000) as i64,
                Ordering::AcqRel,
            );
        }
    }
}
async fn emit(pipe: &Pipe, event: Event) -> Result<(), Problem> {
    timeout(SEND_TIMEOUT, pipe.tx.send(event))
        .await
        .map_err(|_| {
            pipe.denied.store(true, Ordering::Release);
            Problem::Unavailable
        })?
        .map_err(|_| Problem::Unavailable)
}
async fn produce(
    state: &StreamState,
    caller: &Authenticated,
    op: OperationId,
    mut cursor: Cursor,
    pipe: &Pipe,
) -> Result<(), Problem> {
    let mut done = [false; 2];
    let mut index = 0;
    let mut previous: Option<StreamView> = None;
    let mut stats = [m::Output::default(), m::Output::default()];
    loop {
        let before = view(state, caller, op).await?;
        pipe.retain_until(before.expires_at);
        if previous
            .as_ref()
            .is_some_and(|p| matches!(p.source, StreamSource::Live { .. }))
            && matches!(before.source, StreamSource::Archived(_))
        {
            // Reconfirm both stream ends against the newly selected final objects.
            done = [false; 2];
        }
        previous = Some(before.clone());
        if !cursor.matches(owner(&before)?)? {
            return Err(Problem::OutputMissing);
        }
        if done[index] {
            index = 1 - index;
        }
        let loaded = load(state, &before, index, cursor.offsets[index]).await;
        let after = view(state, caller, op).await;
        // No awaited metadata query may follow this last credential check.
        authorize(state, caller).await?;
        let after = after?;
        pipe.retain_until(after.expires_at);
        if !cursor.matches(owner(&after)?)? {
            return Err(Problem::OutputMissing);
        }
        if !same_source(&before, &after) {
            // Publication can win during a live read: discard those bytes and
            // resolve the selected immutable objects at the same cursor.
            if matches!(before.source, StreamSource::Live { .. })
                && matches!(after.source, StreamSource::Archived(_))
            {
                continue;
            }
            return Err(Problem::Unavailable);
        }
        let chunk = loaded?;
        if now() >= chunk.expires {
            return Err(if matches!(before.source, StreamSource::Live { .. }) {
                Problem::Unavailable
            } else {
                Problem::OutputExpired
            });
        }
        done[index] = chunk.complete && chunk.at_end;
        stats[index] = chunk.stats.clone();
        cursor.offsets[index] = chunk.next;
        if !chunk.bytes.is_empty() || done[index] {
            let data = json!({"stream":if index==0 {"stdout"}else{"stderr"},"offset":chunk.offset,"next_offset":chunk.next,
                "data_base64":STANDARD.encode(&chunk.bytes),"at_end":chunk.at_end,"complete":chunk.complete,
                "seen":chunk.stats.seen,"stored":chunk.stats.stored,"truncated":chunk.stats.truncated,"simulated":chunk.simulated,"guest_reported":true});
            emit(
                pipe,
                Event::default()
                    .event("output")
                    .id(cursor.encode()?)
                    .json_data(data)
                    .map_err(|_| Problem::Internal)?,
            )
            .await?;
        }
        if done.iter().all(|v| *v) {
            emit(pipe,Event::default().event("end").id(cursor.encode()?).json_data(json!({"reason":"complete","stdout":stats[0],"stderr":stats[1],"simulated":chunk.simulated,"guest_reported":true})).map_err(|_|Problem::Internal)?).await?;
            return Ok(());
        }
        let idle = chunk.bytes.is_empty();
        index = 1 - index;
        if idle {
            sleep(POLL_INTERVAL).await;
        }
    }
}
struct Session {
    rx: mpsc::Receiver<Event>,
    task: JoinHandle<()>,
    denied: Arc<AtomicBool>,
    expires: Arc<AtomicI64>,
    _permit: OwnedSemaphorePermit,
}
impl Stream for Session {
    type Item = Result<Event, Infallible>;
    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        if this.denied.load(Ordering::Acquire) {
            return Poll::Ready(None);
        }
        if now() >= this.expires.load(Ordering::Acquire) {
            // A bounded queue is still a disclosure boundary: do not release a
            // frame authorized before its retained history expired.
            this.denied.store(true, Ordering::Release);
            this.task.abort();
            return Poll::Ready(Some(Ok(Event::default()
                .event("gap")
                .data("{\"code\":\"output_expired\"}"))));
        }
        this.rx.poll_recv(cx).map(|v| v.map(Ok))
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        self.task.abort();
    }
}
async fn open(
    State(state): State<StreamState>,
    caller: Authenticated,
    path: Result<Path<String>, axum::extract::rejection::PathRejection>,
    query: Result<Query<Parameters>, axum::extract::rejection::QueryRejection>,
    headers: HeaderMap,
) -> Result<Response, Problem> {
    let Path(op) = path.map_err(|_| Problem::BadRequest("invalid operation id"))?;
    let op: OperationId = op
        .parse()
        .map_err(|_| Problem::BadRequest("invalid operation id"))?;
    let Query(query) = query.map_err(|_| Problem::BadRequest("invalid stream query"))?;
    let ids: Vec<_> = headers.get_all("last-event-id").iter().collect();
    if ids.len() > 1
        || !ids.is_empty() && query.cursor.is_some()
        || headers.contains_key(header::RANGE)
    {
        return Err(Problem::BadRequest("ambiguous stream cursor"));
    }
    let raw = ids
        .first()
        .map(|v| v.to_str().map(str::to_owned))
        .transpose()
        .map_err(|_| Problem::BadRequest("invalid stream cursor"))?
        .or(query.cursor);
    let v = view(&state, &caller, op).await?;
    authorize(&state, &caller).await?;
    let owner = owner(&v)?;
    let cursor = raw
        .as_deref()
        .map(|s| Cursor::parse(s, owner))
        .unwrap_or_else(|| Cursor::new(owner))?;
    match v.source {
        StreamSource::Live { .. } if state.live.is_none() => return Err(Problem::Unavailable),
        StreamSource::Archived(_) if state.archive.is_none() => return Err(Problem::Unavailable),
        _ => {}
    }
    let permit = state
        .slots
        .clone()
        .try_acquire_owned()
        .map_err(|_| Problem::Unavailable)?;
    let (tx, rx) = mpsc::channel(1);
    let denied = Arc::new(AtomicBool::new(false));
    let stopped = denied.clone();
    let expires = Arc::new(AtomicI64::new(i64::MAX));
    let pipe = Pipe {
        tx,
        denied: stopped.clone(),
        expires: expires.clone(),
    };
    pipe.retain_until(v.expires_at);
    let task = tokio::spawn(async move {
        let watchdog = async {
            loop {
                sleep(AUTH_INTERVAL).await;
                if authorize(&state, &caller).await.is_err() {
                    stopped.store(true, Ordering::Release);
                    return;
                }
            }
        };
        let result = tokio::select! {
            _=pipe.tx.closed()=>return,
            _=watchdog=>return,
            _=sleep(STREAM_LIFETIME)=>{stopped.store(true,Ordering::Release);return;},
            result=produce(&state,&caller,op,cursor,&pipe)=>result,
        };
        if stopped.load(Ordering::Acquire) {
            return;
        }
        if let Err(problem) = result {
            if matches!(
                problem,
                Problem::Unauthenticated | Problem::Forbidden | Problem::NotFound
            ) {
                stopped.store(true, Ordering::Release);
                return;
            }
            let event = if matches!(problem, Problem::OutputMissing | Problem::OutputExpired) {
                "gap"
            } else {
                "error"
            };
            // Recheck before revealing even the availability of previously authorized output.
            if authorize(&state, &caller).await.is_err() {
                stopped.store(true, Ordering::Release);
                return;
            }
            if let Ok(frame) = Event::default()
                .event(event)
                .json_data(json!({"code":problem.code()}))
            {
                let _ = emit(&pipe, frame).await;
            }
        }
    });
    let response = Sse::new(Session {
        rx,
        task,
        denied,
        expires,
        _permit: permit,
    })
    .keep_alive(
        axum::response::sse::KeepAlive::new()
            .interval(Duration::from_secs(10))
            .text("keepalive"),
    )
    .into_response();
    let mut response = no_store(response);
    response
        .headers_mut()
        .insert("x-accel-buffering", http::HeaderValue::from_static("no"));
    response.headers_mut().insert(
        header::X_CONTENT_TYPE_OPTIONS,
        http::HeaderValue::from_static("nosniff"),
    );
    Ok(response)
}
pub fn routes(
    store: Store,
    live: Option<Arc<dyn live::LiveReader>>,
    archive: Option<Arc<dyn OutputReader>>,
) -> Router {
    Router::new()
        .route("/v1/operations/{id}/stream", get(open))
        .with_state(StreamState {
            store,
            live,
            archive,
            slots: Arc::new(Semaphore::new(4)),
        })
}
