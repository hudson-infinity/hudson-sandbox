#![cfg(unix)]
#![allow(clippy::unwrap_used)]
#[path = "../../sandbox-protocol/tests/support/guest_tls.rs"]
mod certs;
use sandbox_protocol::{Id, OperationId, guest as w, guest_model as m, guest_wire as wire};
use sandbox_supervisor::guest::GuestClient;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::UnixListener,
};

async fn fake(
    mode: u8,
    output: bool,
) -> (GuestClient, tokio::task::JoinHandle<()>, tempfile::TempDir) {
    let certs = certs::Fixture::new();
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("vsock.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let context = m::Context {
        allocation_id: certs.allocation,
        generation: 1,
        boot_id: "00000000-0000-4000-8000-000000000001".into(),
    };
    let client = GuestClient::new(socket, 52, certs.client(), context.clone()).unwrap();
    let tls = certs.server();
    let task = tokio::spawn(async move {
        let (mut io, _) = listener.accept().await.unwrap();
        let mut header = [0; 11];
        io.read_exact(&mut header).await.unwrap();
        assert_eq!(&header, b"CONNECT 52\n");
        if mode == 20 {
            io.write_all(b"OK not-a-port\n").await.unwrap();
            return;
        }
        if mode == 21 {
            io.write_all(b"OK 999999999999999999999999\n")
                .await
                .unwrap();
            return;
        }
        io.write_all(b"OK 1073741824\n").await.unwrap();
        let mut io = tls.accept(io).await.unwrap();
        let request: w::Request = wire::read_frame(&mut io).await.unwrap();
        let operation = match request.action.unwrap() {
            w::request::Action::Inspect(v) => v.operation_id,
            w::request::Action::Output(v) => v.operation_id,
            _ => panic!("unexpected test action"),
        };
        let receipt = m::Receipt {
            version: 1,
            context: context.clone(),
            operation_id: operation.parse().unwrap(),
            digest: [0; 32],
            state: m::State::Exited,
            deadline_unix_ms: 1,
            output_limit: 1024,
            cancel_requested: false,
            cleanup_confirmed: true,
            exit_code: Some(0),
            signal: None,
            stdout: m::Output::default(),
            stderr: m::Output::default(),
            reason: None,
        };
        let mut response = w::Response {
            version: 1,
            request_id: request.request_id,
            context: Some((&context).into()),
            result: Some(if output {
                w::response::Result::Output(w::OutputChunk {
                    operation_id: operation,
                    stream: w::Stream::Stdout as i32,
                    offset: 0,
                    data: b"ok".to_vec(),
                    next_offset: 2,
                    at_end: true,
                    complete: true,
                })
            } else {
                w::response::Result::Receipt((&receipt).into())
            }),
        };
        match mode {
            1 => response.context.as_mut().unwrap().generation = 2,
            2 => response.request_id = OperationId::generate().to_string(),
            3 => response.version = 2,
            4 => response.result = None,
            5 => response.result = Some(w::response::Result::Error(w::Error { code: 0 })),
            _ => {}
        }
        if let Some(w::response::Result::Receipt(ref mut r)) = response.result {
            match mode {
                6 => r.state = 0,
                7 => r.cleanup_confirmed = false,
                8 => r.digest = vec![0; 31],
                9 => r.operation_id = OperationId::generate().to_string(),
                10 => r.signal = Some(9),
                11 => r.stdout.as_mut().unwrap().stored = 2000,
                _ => {}
            }
        }
        if let Some(w::response::Result::Output(ref mut r)) = response.result {
            match mode {
                6 => r.next_offset = 20,
                7 => r.stream = w::Stream::Stderr as i32,
                8 => r.data = vec![0; 33],
                9 => r.offset = 2,
                10 => r.operation_id = OperationId::generate().to_string(),
                _ => {}
            }
        }
        let _ = wire::write_frame(&mut io, &response).await;
    });
    (client, task, dir)
}
#[tokio::test]
async fn validates_receipt_correlation_context_shape_and_connect_ack() {
    for mode in [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 20, 21] {
        let (client, task, _dir) = fake(mode, false).await;
        let result = client.inspect(OperationId::generate()).await;
        assert_eq!(result.is_ok(), mode == 0, "mode {mode}: {result:?}");
        task.await.unwrap();
    }
}
#[tokio::test]
async fn rejects_output_cursor_stream_id_and_size_mismatches() {
    for mode in [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10] {
        let (client, task, _dir) = fake(mode, true).await;
        let result = client
            .output(w::ReadOutput {
                operation_id: OperationId::generate().to_string(),
                stream: w::Stream::Stdout as i32,
                offset: 0,
                limit: 32,
            })
            .await;
        assert_eq!(result.is_ok(), mode == 0, "mode {mode}: {result:?}");
        task.await.unwrap();
    }
}

#[tokio::test]
async fn retirement_requires_exact_authenticated_acknowledgement_and_never_retries_loss() {
    use sandbox_protocol::history::{Barrier, Domain};
    for mode in 0..11 {
        let certs = certs::Fixture::new();
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("history.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let context = m::Context {
            allocation_id: certs.allocation,
            generation: 1,
            boot_id: "boot".into(),
        };
        let client = GuestClient::new(socket, 52, certs.client(), context.clone()).unwrap();
        let barrier = Barrier {
            version: 1,
            context: context.clone(),
            domain: Domain::Files,
            through: OperationId::generate(),
        };
        let tls = certs.server();
        let task = tokio::spawn(async move {
            let (mut io, _) = listener.accept().await.unwrap();
            let mut header = [0; 11];
            io.read_exact(&mut header).await.unwrap();
            io.write_all(b"OK 52\n").await.unwrap();
            let mut io = tls.accept(io).await.unwrap();
            let request: w::Request = wire::read_frame(&mut io).await.unwrap();
            let w::request::Action::RetireHistory(mut b) = request.action.unwrap() else {
                panic!("unexpected request")
            };
            if mode == 10 {
                return;
            } // Lost acknowledgement; no automatic retry.
            match mode {
                1 => b.version = 2,
                2 => b.context.as_mut().unwrap().boot_id = "other".into(),
                3 => b.context.as_mut().unwrap().generation += 1,
                4 => b.domain = w::HistoryDomain::Commands as i32,
                5 => b.through = "op_019a9fad-3000-7000-8000-000000000001".into(),
                6 => b.through = "op_ffffffff-ffff-7fff-bfff-ffffffffffff".into(),
                7 => b.domain = 0,
                _ => {}
            }
            let mut response = w::Response {
                version: 1,
                request_id: request.request_id,
                context: Some((&context).into()),
                result: Some(w::response::Result::HistoryRetired(b)),
            };
            if mode == 8 {
                response.request_id = OperationId::generate().to_string();
            }
            if mode == 9 {
                response.result = Some(w::response::Result::Hello(w::Hello {}));
            }
            wire::write_frame(&mut io, &response).await.unwrap();
        });
        assert_eq!(
            client.retire_history(&barrier).await.is_ok(),
            mode == 0,
            "mode {mode}"
        );
        task.await.unwrap();
    }
}
