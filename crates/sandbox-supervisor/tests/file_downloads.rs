//! Capture ticket ownership, bounded retention and forged-response validation without KVM.
#![allow(clippy::unwrap_used)]
use sandbox_protocol::{
    AllocationId, HostId, Id, OperationId, ProjectId, SandboxId,
    file_downloads::{self as model, ReadScope},
    guest,
    guest_model::Context,
    supervisor::*,
};
use sandbox_supervisor::file_downloads::{MAX_ALLOCATION_DOWNLOADS, MAX_DOWNLOADS, Registry, TTL};
use std::time::Instant;
fn fixture() -> (ReadScope, Context, guest::FileCapture) {
    let s = ReadScope {
        version: 1,
        host_id: HostId::generate(),
        host_epoch: 1,
        project_id: ProjectId::generate(),
        sandbox_id: SandboxId::generate(),
        allocation_id: AllocationId::generate(),
        generation: 1,
    };
    let c = Context {
        allocation_id: s.allocation_id,
        generation: 1,
        boot_id: "original-boot".into(),
    };
    let capture = guest::FileCapture {
        capture_id: OperationId::generate().to_string(),
        path: "private-path".into(),
        size: 4,
        sha256: vec![1; 32],
        expires_unix_ms: 61000,
    };
    (s, c, capture)
}
#[test]
fn tickets_bind_all_owner_and_capture_fields_and_cannot_reopen_after_expiry() {
    let (scope, context, capture) = fixture();
    let now = Instant::now();
    let mut registry = Registry::default();
    let id = registry
        .reserve(scope.clone(), context, capture.path.clone(), 1000, now)
        .unwrap();
    let handle = registry.finish(id, capture, now).unwrap();
    assert!(!registry.validate(&scope, &handle, now).unwrap());
    for field in 0..8 {
        let mut altered = handle.clone();
        match field {
            0 => altered.id = OperationId::generate().to_string(),
            1 => altered.context.as_mut().unwrap().boot_id = "other".into(),
            2 => altered.capture.as_mut().unwrap().capture_id = OperationId::generate().to_string(),
            3 => altered.capture.as_mut().unwrap().path = "other".into(),
            4 => altered.capture.as_mut().unwrap().sha256[0] ^= 1,
            5 => altered.capture.as_mut().unwrap().size += 1,
            6 => altered.expires_unix_ms += 1,
            _ => altered.capture.as_mut().unwrap().expires_unix_ms += 1,
        }
        assert!(registry.validate(&scope, &altered, now).is_err());
    }
    for field in 0..6 {
        let mut altered = scope.clone();
        match field {
            0 => altered.host_id = HostId::generate(),
            1 => altered.host_epoch += 1,
            2 => altered.project_id = ProjectId::generate(),
            3 => altered.sandbox_id = SandboxId::generate(),
            4 => altered.allocation_id = AllocationId::generate(),
            _ => altered.generation += 1,
        }
        assert!(registry.validate(&altered, &handle, now).is_err());
    }
    registry.released(&scope, &handle, now).unwrap();
    assert!(registry.validate(&scope, &handle, now).unwrap());
    registry.released(&scope, &handle, now).unwrap();
    assert!(registry.validate(&scope, &handle, now + TTL).is_err());
    assert!(Registry::default().validate(&scope, &handle, now).is_err());
}
#[test]
fn pending_and_released_tickets_count_until_monotonic_expiry_at_both_bounds() {
    let (scope, context, capture) = fixture();
    let now = Instant::now();
    let mut registry = Registry::default();
    for _ in 0..MAX_ALLOCATION_DOWNLOADS {
        let id = registry
            .reserve(
                scope.clone(),
                context.clone(),
                capture.path.clone(),
                1000,
                now,
            )
            .unwrap();
        let h = registry.finish(id, capture.clone(), now).unwrap();
        registry.released(&scope, &h, now).unwrap();
    }
    assert!(
        registry
            .reserve(
                scope.clone(),
                context.clone(),
                capture.path.clone(),
                1000,
                now
            )
            .is_err()
    );
    for _ in MAX_ALLOCATION_DOWNLOADS..MAX_DOWNLOADS {
        let (s, c, g) = fixture();
        registry.reserve(s, c, g.path, 1000, now).unwrap();
    }
    let (s, c, g) = fixture();
    assert!(registry.reserve(s, c, g.path, 1000, now).is_err());
    assert!(
        registry
            .reserve(scope, context, capture.path, 61000, now + TTL)
            .is_ok()
    );
}
#[test]
fn forged_scope_provenance_capture_and_chunk_responses_are_rejected() {
    let (scope, context, capture) = fixture();
    let r = FileCaptureRequest {
        scope_json: serde_json::to_vec(&scope).unwrap(),
        path: capture.path.clone(),
        expires_unix_ms: 2000,
    };
    let h = FileDownloadHandle {
        id: OperationId::generate().to_string(),
        context: Some((&context).into()),
        capture: Some(capture),
        expires_unix_ms: 61000,
    };
    let o = FileAccessObservation {
        scope_json: r.scope_json.clone(),
        simulated: false,
        observed_unix_ms: 1000,
        result: Some(file_access_observation::Result::Captured(h.clone())),
    };
    assert_eq!(model::captured(&r, &o, false, 1000).unwrap(), h);
    let mut bad = o.clone();
    bad.simulated = true;
    assert!(model::captured(&r, &bad, false, 1000).is_err());
    bad = o.clone();
    bad.scope_json = b"{}".to_vec();
    assert!(model::captured(&r, &bad, false, 1000).is_err());
    bad = o.clone();
    bad.observed_unix_ms = 20000;
    assert!(model::captured(&r, &bad, false, 1000).is_err());
    let read = FileDownloadRequest {
        scope_json: r.scope_json,
        handle: Some(h.clone()),
        offset: 0,
        limit: 4,
        expires_unix_ms: 2000,
    };
    let chunk = guest::FileChunk {
        handle: Some(sandbox_protocol::file_wire::handle(h.capture.as_ref().unwrap()).unwrap()),
        offset: 0,
        data: vec![0, 1, 2, 3],
        next_offset: 4,
        size: 4,
        at_end: true,
    };
    let mut reply = FileAccessObservation {
        scope_json: read.scope_json.clone(),
        simulated: false,
        observed_unix_ms: 1000,
        result: Some(file_access_observation::Result::Chunk(chunk.clone())),
    };
    assert_eq!(model::chunk(&read, &reply, false, 1000).unwrap(), chunk);
    for field in 0..7 {
        let mut bad = chunk.clone();
        match field {
            0 => bad.handle.as_mut().unwrap().capture_id = OperationId::generate().to_string(),
            1 => bad.handle.as_mut().unwrap().sha256[0] ^= 1,
            2 => bad.offset += 1,
            3 => bad.data.truncate(3),
            4 => bad.next_offset += 1,
            5 => bad.size += 1,
            _ => bad.at_end = false,
        }
        reply.result = Some(file_access_observation::Result::Chunk(bad));
        assert!(model::chunk(&read, &reply, false, 1000).is_err());
    }
    assert!(!format!("{o:?} {h:?}").contains("private-path"));
}

#[tokio::test(start_paused = true)]
async fn detached_download_callers_keep_all_slots_until_worker_deadline() {
    use sandbox_supervisor::file_downloads::{CALL_TIMEOUT, Workers};
    let workers = Workers::default();
    let (entered, mut ready) = tokio::sync::mpsc::channel(4);
    let mut callers = Vec::new();
    for _ in 0..4 {
        let worker = workers.clone();
        let entered = entered.clone();
        callers.push(tokio::spawn(async move {
            worker
                .run(async move {
                    entered.send(()).await.unwrap();
                    std::future::pending::<Result<(), tonic::Status>>().await
                })
                .await
        }));
    }
    for _ in 0..4 {
        ready.recv().await.unwrap();
    }
    for caller in callers {
        caller.abort();
        let _ = caller.await;
    }
    assert_eq!(
        workers.run(async { Ok(()) }).await.unwrap_err().code(),
        tonic::Code::ResourceExhausted
    );
    tokio::time::advance(CALL_TIMEOUT).await;
    // Deadline completion is scheduled independently of the abandoned callers.
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
    workers.run(async { Ok(()) }).await.unwrap();
}
