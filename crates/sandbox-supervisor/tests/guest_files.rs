#![cfg(unix)]
#![allow(clippy::unwrap_used)]
#[path = "../../sandbox-protocol/tests/support/guest_tls.rs"]
mod certs;
use sandbox_protocol::{
    Id, OperationId, file_wire as wire, files as m, guest as w, guest_model::Context, guest_wire,
};
use sandbox_supervisor::guest::GuestClient;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::UnixListener,
};

fn upload() -> m::Upload {
    m::Upload {
        operation_id: OperationId::generate(),
        path: "file".into(),
        size: 4,
        sha256: [3; 32],
        mode: 0o644,
    }
}
async fn fake(
    reply: impl FnOnce(w::Request, Context) -> w::response::Result + Send + 'static,
) -> (GuestClient, tokio::task::JoinHandle<()>, tempfile::TempDir) {
    let certs = certs::Fixture::new();
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("vsock.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let context = Context {
        allocation_id: certs.allocation,
        generation: 1,
        boot_id: "boot".into(),
    };
    let client = GuestClient::new(socket, 52, certs.client(), context.clone()).unwrap();
    let tls = certs.server();
    let task = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut header = [0; 11];
        stream.read_exact(&mut header).await.unwrap();
        assert_eq!(&header, b"CONNECT 52\n");
        stream.write_all(b"OK 12345\n").await.unwrap();
        let mut stream = tls.accept(stream).await.unwrap();
        let request: w::Request = guest_wire::read_frame(&mut stream).await.unwrap();
        let request_id = request.request_id.clone();
        let result = reply(request, context.clone());
        guest_wire::write_frame(
            &mut stream,
            &w::Response {
                version: 1,
                request_id,
                context: Some((&context).into()),
                result: Some(result),
            },
        )
        .await
        .unwrap();
    });
    (client, task, dir)
}
#[tokio::test]
async fn file_receipts_must_match_the_original_upload_and_boot() {
    for mode in 0..8 {
        let input = upload();
        let saved = input.clone();
        let (client, task, _dir) = fake(move |request, context| {
            assert!(matches!(
                request.action,
                Some(w::request::Action::BeginUpload(_))
            ));
            let mut receipt = m::Receipt {
                version: 1,
                context,
                digest: saved.digest().unwrap(),
                upload: saved,
                state: m::State::Staging,
            };
            match mode {
                1 => receipt.context.boot_id = "other".into(),
                2 => receipt.upload.operation_id = OperationId::generate(),
                3 => receipt.upload.path = "different".into(),
                4 => receipt.upload.size += 1,
                5 => receipt.upload.sha256 = [5; 32],
                6 => receipt.upload.mode = 0o755,
                7 => receipt.context.generation += 1,
                _ => {}
            }
            // Even internally consistent alternate descriptors must fail comparison with the request.
            receipt.digest = receipt.upload.digest().unwrap();
            w::response::Result::FileReceipt((&receipt).into())
        })
        .await;
        assert_eq!(
            client.begin_upload(&input).await.is_ok(),
            mode == 0,
            "mode {mode}"
        );
        task.await.unwrap();
    }
}
#[tokio::test]
async fn file_progress_cannot_change_identity_or_claim_unbounded_bytes() {
    for mode in 0..5 {
        let (client, task, _dir) = fake(move |request, _| {
            let Some(w::request::Action::WriteFile(v)) = request.action else {
                panic!()
            };
            let mut progress = w::FileProgress {
                operation: v.operation,
                stored: 2,
            };
            match mode {
                1 => {
                    progress.operation.as_mut().unwrap().operation_id =
                        OperationId::generate().to_string()
                }
                2 => progress.operation.as_mut().unwrap().digest = vec![9; 32],
                3 => progress.stored = 1,
                4 => progress.stored = 5,
                _ => {}
            }
            w::response::Result::FileProgress(progress)
        })
        .await;
        assert_eq!(
            client.write_file(&upload(), 0, b"ab").await.is_ok(),
            mode == 0
        );
        task.await.unwrap();
    }
}
#[tokio::test]
async fn capture_descriptors_must_match_path_and_file_bounds() {
    for mode in 0..6 {
        let (client, task, _dir) = fake(move |_, _| {
            let mut capture = w::FileCapture {
                capture_id: OperationId::generate().to_string(),
                path: "file".into(),
                size: 4,
                sha256: vec![3; 32],
                expires_unix_ms: 1,
            };
            match mode {
                1 => capture.path = "other".into(),
                2 => capture.size = m::MAX_FILE_BYTES + 1,
                3 => capture.sha256.clear(),
                4 => capture.capture_id = "invalid".into(),
                5 => capture.expires_unix_ms = 0,
                _ => {}
            }
            w::response::Result::FileCapture(capture)
        })
        .await;
        assert_eq!(client.capture_file("file").await.is_ok(), mode == 0);
        task.await.unwrap();
    }
}
#[tokio::test]
async fn captured_chunks_must_match_handle_digest_range_size_and_end() {
    for mode in 0..10 {
        let descriptor = w::FileCapture {
            capture_id: OperationId::generate().to_string(),
            path: "file".into(),
            size: 4,
            sha256: vec![3; 32],
            expires_unix_ms: 1,
        };
        let (client, task, _dir) = fake(move |request, _| {
            let Some(w::request::Action::ReadFile(v)) = request.action else {
                panic!()
            };
            let mut chunk = w::FileChunk {
                handle: v.handle,
                offset: 1,
                data: b"bc".to_vec(),
                next_offset: 3,
                size: 4,
                at_end: false,
            };
            match mode {
                1 => chunk.offset = 0,
                2 => chunk.next_offset = 4,
                3 => chunk.at_end = true,
                4 => chunk.size = 5,
                5 => chunk.data.clear(),
                6 => chunk.data.push(1),
                7 => {
                    chunk.handle.as_mut().unwrap().capture_id = OperationId::generate().to_string()
                }
                8 => chunk.handle.as_mut().unwrap().sha256 = vec![8; 32],
                9 => chunk.handle = None,
                _ => {}
            }
            w::response::Result::FileChunk(chunk)
        })
        .await;
        assert_eq!(
            client.read_file(&descriptor, 1, 2).await.is_ok(),
            mode == 0,
            "mode {mode}"
        );
        task.await.unwrap();
    }
}
#[tokio::test]
async fn release_replies_cannot_acknowledge_a_different_capture() {
    for changed in [false, true] {
        let descriptor = w::FileCapture {
            capture_id: OperationId::generate().to_string(),
            path: "file".into(),
            size: 4,
            sha256: vec![3; 32],
            expires_unix_ms: 1,
        };
        let (client, task, _dir) = fake(move |request, _| {
            let Some(w::request::Action::ReleaseFile(mut v)) = request.action else {
                panic!()
            };
            if changed {
                v.sha256 = vec![9; 32];
            }
            w::response::Result::FileReleased(v)
        })
        .await;
        assert_eq!(client.release_file(&descriptor).await.is_ok(), !changed);
        task.await.unwrap();
        wire::handle(&descriptor).unwrap();
    }
}
