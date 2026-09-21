#![allow(clippy::unwrap_used)]
use super::*;
use sandbox_protocol::AllocationId;
use sha2::{Digest, Sha256};
use w::{request::Action as A, response::Result as R};
async fn fixture() -> (FileService, tempfile::TempDir, Context) {
    let dir = tempfile::tempdir().unwrap();
    let context = Context {
        allocation_id: AllocationId::generate(),
        generation: 1,
        boot_id: "test-boot".into(),
    };
    let service = FileService::open(dir.path().into(), context.clone())
        .await
        .unwrap();
    (service, dir, context)
}
fn input(data: &[u8]) -> m::Upload {
    m::Upload {
        operation_id: OperationId::generate(),
        path: "file".into(),
        size: data.len() as u64,
        sha256: Sha256::digest(data).into(),
        mode: 0o644,
    }
}
async fn capture(service: &FileService) -> w::FileCapture {
    let R::FileCapture(v) = service
        .call(A::CaptureFile(w::CaptureFile {
            path: "file".into(),
        }))
        .await
        .unwrap()
    else {
        panic!()
    };
    v
}
#[tokio::test]
async fn upload_transport_checks_digest_and_retains_commit_across_retry() {
    let (service, dir, _) = fixture().await;
    let input = input(b"payload");
    let operation = wire::operation(&input).unwrap();
    let R::FileReceipt(first) = service.call(A::BeginUpload((&input).into())).await.unwrap() else {
        panic!()
    };
    assert_eq!(first.state, w::FileState::Staging as i32);
    let write = w::WriteFile {
        operation: Some(operation.clone()),
        offset: 0,
        data: b"payload".to_vec(),
    };
    let mut wrong = write.clone();
    wrong.operation.as_mut().unwrap().digest[0] ^= 1;
    assert!(service.call(A::WriteFile(wrong)).await.is_err());
    for _ in 0..2 {
        let R::FileProgress(v) = service.call(A::WriteFile(write.clone())).await.unwrap() else {
            panic!()
        };
        assert_eq!(v.stored, 7);
    }
    let R::FileReceipt(done) = service
        .call(A::CommitUpload(operation.clone()))
        .await
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(done.state, w::FileState::Committed as i32);
    std::fs::write(dir.path().join("file"), b"later").unwrap();
    let R::FileReceipt(retry) = service
        .call(A::CommitUpload(operation.clone()))
        .await
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(retry, done);
    assert_eq!(std::fs::read(dir.path().join("file")).unwrap(), b"later");
    let mut wrong = operation;
    wrong.digest[0] ^= 1;
    assert!(service.call(A::InspectUpload(wrong)).await.is_err());
    service.shutdown().await.unwrap();
    assert!(service.call(A::BeginUpload((&input).into())).await.is_err());
}
#[tokio::test]
async fn captures_are_stable_bounded_releasable_and_expire_without_a_request() {
    let (service, dir, _) = fixture().await;
    std::fs::write(dir.path().join("file"), b"original").unwrap();
    let mut captures = Vec::new();
    for _ in 0..8 {
        captures.push(capture(&service).await);
    }
    assert!(
        service
            .call(A::CaptureFile(w::CaptureFile {
                path: "file".into()
            }))
            .await
            .is_err()
    );
    let descriptor = &captures[0];
    let handle = wire::handle(descriptor).unwrap();
    std::fs::write(dir.path().join("file"), b"changed").unwrap();
    let read = w::ReadFile {
        handle: Some(handle.clone()),
        offset: 0,
        limit: 8,
    };
    let R::FileChunk(bytes) = service.call(A::ReadFile(read.clone())).await.unwrap() else {
        panic!()
    };
    assert_eq!(bytes.data, b"original");
    let mut wrong = read.clone();
    wrong.handle.as_mut().unwrap().sha256[0] ^= 1;
    assert!(service.call(A::ReadFile(wrong)).await.is_err());
    for _ in 0..2 {
        service.call(A::ReleaseFile(handle.clone())).await.unwrap();
    }
    assert!(service.call(A::ReadFile(read.clone())).await.is_err());
    let fresh = capture(&service).await;
    assert_ne!(fresh.capture_id, descriptor.capture_id);
    let id = fresh.capture_id.parse::<OperationId>().unwrap();
    {
        service
            .0
            .state
            .lock()
            .unwrap()
            .captures
            .get_mut(&id)
            .unwrap()
            .until = Instant::now() - Duration::from_secs(1);
    }
    tokio::time::sleep(Duration::from_millis(1200)).await;
    assert!(
        !service.0.state.lock().unwrap().captures.contains_key(&id),
        "idle reaper did not expire the capture"
    );
    assert!(
        service
            .call(A::ReadFile(w::ReadFile {
                handle: Some(wire::handle(&fresh).unwrap()),
                offset: 0,
                limit: 8
            }))
            .await
            .is_err()
    );
    service.shutdown().await.unwrap();
}
#[tokio::test]
async fn restart_keeps_upload_receipts_but_cannot_recapture_an_old_download_handle() {
    let (service, dir, context) = fixture().await;
    let input = input(b"");
    let operation = wire::operation(&input).unwrap();
    service.call(A::BeginUpload((&input).into())).await.unwrap();
    service
        .call(A::CommitUpload(operation.clone()))
        .await
        .unwrap();
    let old = capture(&service).await;
    service.shutdown().await.unwrap();
    drop(service);
    std::fs::write(dir.path().join("file"), b"new bytes").unwrap();
    let service = FileService::open(dir.path().into(), context).await.unwrap();
    let R::FileReceipt(receipt) = service.call(A::InspectUpload(operation)).await.unwrap() else {
        panic!()
    };
    assert_eq!(receipt.state, w::FileState::Committed as i32);
    assert!(
        service
            .call(A::ReadFile(w::ReadFile {
                handle: Some(wire::handle(&old).unwrap()),
                offset: 0,
                limit: 8
            }))
            .await
            .is_err()
    );
    assert_ne!(capture(&service).await.capture_id, old.capture_id);
    service.shutdown().await.unwrap();
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn worker_timeout_keeps_capacity_until_the_blocking_operation_finishes() {
    let (service, _dir, _) = fixture().await;
    let (started, observed) = tokio::sync::oneshot::channel();
    let (release, blocked) = std::sync::mpsc::channel();
    let active = service.clone();
    let task = tokio::spawn(async move {
        active
            .run(move |_| {
                let _ = started.send(());
                blocked.recv().unwrap();
                Ok(())
            })
            .await
    });
    observed.await.unwrap();
    assert!(task.await.unwrap().is_err()); // Five-second caller deadline, worker is still alive.
    assert!(
        service
            .call(A::CaptureFile(w::CaptureFile {
                path: "file".into()
            }))
            .await
            .unwrap_err()
            .to_string()
            .contains("busy")
    );
    release.send(()).unwrap();
    service.shutdown().await.unwrap();
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn disconnected_caller_does_not_free_an_admitted_worker() {
    let (service, _dir, _) = fixture().await;
    let (started, observed) = tokio::sync::oneshot::channel();
    let (release, blocked) = std::sync::mpsc::channel();
    let active = service.clone();
    let task = tokio::spawn(async move {
        active
            .run(move |_| {
                let _ = started.send(());
                blocked.recv().unwrap();
                Ok(())
            })
            .await
    });
    observed.await.unwrap();
    task.abort();
    let _ = task.await;
    assert!(
        service
            .call(A::CaptureFile(w::CaptureFile {
                path: "file".into()
            }))
            .await
            .unwrap_err()
            .to_string()
            .contains("busy")
    );
    release.send(()).unwrap();
    service.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_reports_an_undrained_worker_and_fences_further_calls() {
    let (service, _dir, _) = fixture().await;
    let (started, observed) = tokio::sync::oneshot::channel();
    let (release, blocked) = std::sync::mpsc::channel();
    let active = service.clone();
    let task = tokio::spawn(async move {
        active
            .run(move |_| {
                let _ = started.send(());
                blocked.recv().unwrap();
                Ok(())
            })
            .await
    });
    observed.await.unwrap();
    let stopped = service.shutdown().await;
    assert!(stopped.unwrap_err().to_string().contains("unconfirmed"));
    assert!(
        service
            .call(A::CaptureFile(w::CaptureFile {
                path: "file".into()
            }))
            .await
            .unwrap_err()
            .to_string()
            .contains("closed")
    );
    release.send(()).unwrap();
    let _ = task.await.unwrap();
    service.shutdown().await.unwrap();
}
