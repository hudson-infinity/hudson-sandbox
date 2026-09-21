#![allow(clippy::unwrap_used)]
#[path = "support/guest_tls.rs"]
mod tls;
use sandbox_protocol::{
    AllocationId, Id, OperationId, guest as w, guest_model as m,
    guest_wire::{self as wire, ClientTls, ServerTls},
};
use std::collections::BTreeMap;
use tokio::io::{AsyncWriteExt, duplex};

#[tokio::test]
async fn frames_round_trip_and_reject_zero_oversize_truncated_and_invalid_protobuf() {
    let (mut a, mut b) = duplex(1024);
    let request = w::Operation {
        operation_id: OperationId::generate().to_string(),
    };
    wire::write_frame(&mut a, &request).await.unwrap();
    let received: w::Operation = wire::read_frame(&mut b).await.unwrap();
    assert_eq!(received, request);
    let binary = w::OutputChunk {
        data: vec![0, 255],
        ..Default::default()
    };
    wire::write_frame(&mut a, &binary).await.unwrap();
    let decoded: w::OutputChunk = wire::read_frame(&mut b).await.unwrap();
    assert_eq!(decoded.data, [0, 255]);
    for bytes in [
        0u32.to_be_bytes().to_vec(),
        ((wire::MAX_FRAME + 1) as u32).to_be_bytes().to_vec(),
        vec![0, 0, 0, 8, 1],
        vec![0, 0, 0, 1, 255],
    ] {
        let (mut a, mut b) = duplex(32);
        a.write_all(&bytes).await.unwrap();
        drop(a);
        assert!(wire::read_frame::<w::Request>(&mut b).await.is_err());
    }
    let (mut a, _) = duplex(32);
    let huge = w::Operation {
        operation_id: "x".repeat(wire::MAX_FRAME),
    };
    assert!(wire::write_frame(&mut a, &huge).await.is_err());
}
#[tokio::test(start_paused = true)]
async fn slow_header_body_and_writer_have_total_deadlines() {
    for bytes in [vec![0], vec![0, 0, 0, 20, 1]] {
        let (mut a, mut b) = duplex(32);
        a.write_all(&bytes).await.unwrap();
        assert!(wire::read_frame::<w::Request>(&mut b).await.is_err());
        drop(a);
    }
    let (mut a, _b) = duplex(1);
    assert!(
        wire::write_frame(
            &mut a,
            &w::Operation {
                operation_id: "value".into()
            }
        )
        .await
        .is_err()
    );
}
async fn exchange(server: ServerTls, client: ClientTls) -> (bool, bool) {
    let (a, b) = duplex(32768);
    let server = tokio::spawn(async move {
        let Ok(mut a) = server.accept(a).await else {
            return false;
        };
        let Ok(v) = wire::read_frame::<w::Operation>(&mut a).await else {
            return false;
        };
        wire::write_frame(&mut a, &v).await.is_ok()
    });
    let result = async {
        let mut b = client.connect(b).await?;
        wire::write_frame(
            &mut b,
            &w::Operation {
                operation_id: "roundtrip".into(),
            },
        )
        .await?;
        let r: w::Operation = wire::read_frame(&mut b).await?;
        anyhow::ensure!(r.operation_id == "roundtrip");
        Ok::<_, anyhow::Error>(())
    }
    .await
    .is_ok();
    (server.await.unwrap(), result)
}
#[tokio::test]
async fn mtls_checks_both_exact_peers_and_allocation_server_name() {
    let f = tls::Fixture::new();
    assert_eq!(exchange(f.server(), f.client()).await, (true, true));
    let other = tls::Leaf::new(&f.ca, "host.sandbox.internal".into(), true);
    assert_eq!(
        exchange(
            f.server(),
            f.client_for(&other, f.guest.pin(), f.allocation)
        )
        .await,
        (false, false)
    );
    assert_eq!(
        exchange(f.server(), f.client_for(&f.host, [1; 32], f.allocation)).await,
        (false, false)
    );
    assert_eq!(
        exchange(
            f.server(),
            f.client_for(&f.host, f.guest.pin(), AllocationId::generate())
        )
        .await,
        (false, false)
    );
    let other_ca = tls::Fixture::new();
    assert_eq!(
        exchange(f.server(), other_ca.client()).await,
        (false, false)
    );
}
#[tokio::test(start_paused = true)]
async fn tls_does_not_accept_plaintext_or_a_stalled_peer() {
    let f = tls::Fixture::new();
    let (a, mut b) = duplex(256);
    b.write_all(b"POST /run HTTP/1.1\r\n\r\n").await.unwrap();
    drop(b);
    assert!(f.server().accept(a).await.is_err());
    let (a, _b) = duplex(256);
    assert!(f.server().accept(a).await.is_err());
}
#[test]
fn typed_requests_roundtrip_without_logging_command_or_output_bytes() {
    let request = m::Execute {
        operation_id: OperationId::generate(),
        argv: vec!["secret-command".into()],
        env: BTreeMap::from([("TOKEN".into(), "secret-token".into())]),
        cwd: "/".into(),
        deadline_unix_ms: 1,
        output_limit: 1024,
    };
    let wire: w::Execute = (&request).into();
    assert!(!format!("{wire:?}").contains("secret"));
    let back: m::Execute = wire.try_into().unwrap();
    assert_eq!(back.digest().unwrap(), request.digest().unwrap());
    let output = w::OutputChunk {
        data: b"private-output".to_vec(),
        ..Default::default()
    };
    assert!(!format!("{output:?}").contains("private"));
    let invalid = w::Receipt {
        version: 1,
        state: w::State::Unspecified as i32,
        ..Default::default()
    };
    assert!(m::Receipt::try_from(invalid).is_err());
}
