use super::*;
use sandbox_protocol::{
    file_downloads::{self as model, ReadScope},
    files as fm,
    supervisor::{
        FileCaptureRequest, FileDownloadHandle, FileDownloadRequest, FileReleaseRequest,
        FileRequest, FileWriteRequest, file_downloads_server::FileDownloads,
    },
};
use sha2::{Digest, Sha256};
fn capture_request(o: &Ownership) -> FileCaptureRequest {
    let s = ReadScope {
        version: 1,
        host_id: o.host_id.parse().unwrap(),
        host_epoch: o.supervisor_epoch,
        project_id: o.project_id.parse().unwrap(),
        sandbox_id: o.sandbox_id.parse().unwrap(),
        allocation_id: o.allocation_id.parse().unwrap(),
        generation: o.generation,
    };
    FileCaptureRequest {
        scope_json: serde_json::to_vec(&s).unwrap(),
        path: "file.bin".into(),
        expires_unix_ms: unix_ms().unwrap() + 30000,
    }
}
async fn publish(fake: &FakeHost, owner: &Ownership, bytes: &[u8]) {
    let mut owner = owner.clone();
    owner.operation_id = OperationId::generate().to_string();
    let upload = fm::Upload {
        operation_id: owner.operation_id.parse().unwrap(),
        path: "file.bin".into(),
        size: bytes.len() as u64,
        sha256: Sha256::digest(bytes).into(),
        mode: 0o644,
    };
    let r = FileRequest {
        ownership: Some(owner),
        upload: Some((&upload).into()),
    };
    fake.begin_file(Request::new(r.clone())).await.unwrap();
    for (i, chunk) in bytes.chunks(fm::MAX_CHUNK_BYTES).enumerate() {
        fake.write_file(Request::new(FileWriteRequest {
            request: Some(r.clone()),
            offset: (i * fm::MAX_CHUNK_BYTES) as u64,
            data: chunk.to_vec(),
        }))
        .await
        .unwrap();
    }
    assert_eq!(
        fake.commit_file(Request::new(r))
            .await
            .unwrap()
            .get_ref()
            .state,
        3
    );
}
async fn capture(fake: &FakeHost, r: &FileCaptureRequest) -> FileDownloadHandle {
    let reply = fake
        .capture(Request::new(r.clone()))
        .await
        .unwrap()
        .into_inner();
    model::captured(r, &reply, true, unix_ms().unwrap()).unwrap()
}
fn read_request(r: &FileCaptureRequest, h: FileDownloadHandle) -> FileDownloadRequest {
    FileDownloadRequest {
        scope_json: r.scope_json.clone(),
        handle: Some(h),
        offset: 0,
        limit: 32768,
        expires_unix_ms: unix_ms().unwrap() + 30000,
    }
}
#[tokio::test]
async fn upload_capture_replacement_reconnect_release_loss_and_stop_preserve_identity() {
    let (fake, create) = fixture();
    fake.create(Request::new(create.clone())).await.unwrap();
    let owner = create.ownership.as_ref().unwrap();
    let bytes: Vec<u8> = (0..65539).map(|n| (n % 251) as u8).collect();
    publish(&fake, owner, &bytes).await;
    let r = capture_request(owner);
    let h = capture(&fake, &r).await;
    publish(&fake, owner, b"changed").await;
    let mut read = read_request(&r, h.clone());
    let mut output = Vec::new();
    while output.len() < bytes.len() {
        read.offset = output.len() as u64;
        let o = fake
            .clone()
            .read(Request::new(read.clone()))
            .await
            .unwrap()
            .into_inner();
        output.extend(
            model::chunk(&read, &o, true, unix_ms().unwrap())
                .unwrap()
                .data,
        );
    }
    assert_eq!(output, bytes);
    assert_eq!(
        Sha256::digest(&output).to_vec(),
        h.capture.as_ref().unwrap().sha256
    );
    let mut forged = read.clone();
    forged
        .handle
        .as_mut()
        .unwrap()
        .capture
        .as_mut()
        .unwrap()
        .path = "other".into();
    assert_eq!(
        fake.read(Request::new(forged)).await.unwrap_err().code(),
        Code::FailedPrecondition
    );
    let mut foreign: ReadScope = serde_json::from_slice(&read.scope_json).unwrap();
    foreign.project_id = ProjectId::generate();
    let mut forged = read.clone();
    forged.scope_json = serde_json::to_vec(&foreign).unwrap();
    assert_eq!(
        fake.read(Request::new(forged)).await.unwrap_err().code(),
        Code::FailedPrecondition
    );
    let release = FileReleaseRequest {
        scope_json: r.scope_json.clone(),
        handle: Some(h.clone()),
        expires_unix_ms: unix_ms().unwrap() + 30000,
    };
    fake.lose_next_download_reply().await;
    assert!(fake.release(Request::new(release.clone())).await.is_err());
    let reply = fake
        .release(Request::new(release.clone()))
        .await
        .unwrap()
        .into_inner();
    model::released(&release, &reply, true, unix_ms().unwrap()).unwrap();
    assert_eq!(
        fake.read(Request::new(read)).await.unwrap_err().code(),
        Code::NotFound
    );
    assert_eq!(fake.total_file_captures().await, 1);
    let fresh = capture(&fake, &r).await;
    assert_ne!(fresh.id, h.id);
    let read = read_request(&r, fresh);
    let reply = fake
        .read(Request::new(read.clone()))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        model::chunk(&read, &reply, true, unix_ms().unwrap())
            .unwrap()
            .data,
        b"changed"
    );
    assert!(!format!("{fake:?}").contains("changed"));
    fake.stop(Request::new(StopRequest {
        ownership: Some(owner.clone()),
    }))
    .await
    .unwrap();
    assert!(fake.read(Request::new(read)).await.is_err());
    assert!(fake.capture(Request::new(r)).await.is_err());
}
#[tokio::test(start_paused = true)]
async fn lost_captures_use_bounded_slots_until_expiry_without_recreating_old_handles() {
    let (fake, mut create) = fixture();
    create.allocation_expires_unix_ms = unix_ms().unwrap() + 120000;
    fake.create(Request::new(create.clone())).await.unwrap();
    let owner = create.ownership.as_ref().unwrap();
    publish(&fake, owner, b"data").await;
    let r = capture_request(owner);
    let h = capture(&fake, &r).await;
    for _ in 1..8 {
        fake.lose_next_download_reply().await;
        assert!(fake.capture(Request::new(r.clone())).await.is_err());
    }
    assert_eq!(fake.total_file_captures().await, 8);
    assert_eq!(
        fake.capture(Request::new(r.clone()))
            .await
            .unwrap_err()
            .code(),
        Code::ResourceExhausted
    );
    tokio::time::advance(std::time::Duration::from_secs(61)).await;
    assert_eq!(
        fake.read(Request::new(read_request(&r, h)))
            .await
            .unwrap_err()
            .code(),
        Code::NotFound
    );
    capture(&fake, &r).await;
    assert_eq!(fake.total_file_captures().await, 9);
}
#[tokio::test]
async fn fake_captured_memory_is_global_and_confirmed_release_frees_bytes() {
    let (fake, create) = fixture();
    fake.create(Request::new(create.clone())).await.unwrap();
    let owner = create.ownership.as_ref().unwrap();
    publish(&fake, owner, &vec![7; fm::MAX_FILE_BYTES as usize]).await;
    let r = capture_request(owner);
    let mut handles = Vec::new();
    for _ in 0..8 {
        handles.push(capture(&fake, &r).await);
    }
    let mut second = create.clone();
    let o = second.ownership.as_mut().unwrap();
    o.allocation_id = AllocationId::generate().to_string();
    o.sandbox_id = SandboxId::generate().to_string();
    o.operation_id = OperationId::generate().to_string();
    fake.create(Request::new(second.clone())).await.unwrap();
    let other = second.ownership.as_ref().unwrap();
    publish(&fake, other, b"x").await;
    let other = capture_request(other);
    assert_eq!(
        fake.capture(Request::new(other.clone()))
            .await
            .unwrap_err()
            .code(),
        Code::ResourceExhausted
    );
    fake.release(Request::new(FileReleaseRequest {
        scope_json: r.scope_json,
        handle: handles.pop(),
        expires_unix_ms: unix_ms().unwrap() + 30000,
    }))
    .await
    .unwrap();
    capture(&fake, &other).await;
}
