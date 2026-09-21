#![allow(clippy::unwrap_used, clippy::expect_used)]
use sandbox_protocol::{
    AllocationId, HostId, Id, OperationId, ProjectId, SandboxId, guest as w, guest_model as m,
    output::{MAX_CHUNK, OutputName, OutputOwner, OutputTicket},
    supervisor::OutputRequest,
};
use sandbox_supervisor::archive::{self, OutputSource};
use tonic::{Code, Status};

#[derive(Debug)]
struct Source {
    operation: OperationId,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    fault: u8,
}
#[tonic::async_trait]
impl OutputSource for Source {
    async fn read(
        &self,
        name: OutputName,
        offset: u64,
        limit: u32,
    ) -> Result<w::OutputChunk, Status> {
        if self.fault == 9 {
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        }
        if self.fault == 10 {
            return Err(Status::not_found("missing output history"));
        }
        let bytes = match name {
            OutputName::Stdout => &self.stdout,
            OutputName::Stderr => &self.stderr,
        };
        let end = bytes.len().min(offset as usize + limit as usize);
        let mut chunk = w::OutputChunk {
            operation_id: self.operation.to_string(),
            stream: match name {
                OutputName::Stdout => w::Stream::Stdout,
                OutputName::Stderr => w::Stream::Stderr,
            } as i32,
            offset,
            data: bytes[offset as usize..end].to_vec(),
            next_offset: end as u64,
            at_end: end == bytes.len(),
            complete: true,
        };
        match self.fault {
            1 => chunk.operation_id = OperationId::generate().to_string(),
            2 => chunk.stream = 0,
            3 => chunk.next_offset += 1,
            4 => chunk.complete = false,
            5 => chunk.at_end = !chunk.at_end,
            6 => {
                chunk.data.clear();
                chunk.next_offset = offset;
                chunk.at_end = false;
            }
            7 => chunk.data = vec![1; MAX_CHUNK + 1],
            8 => chunk.offset += 1,
            _ => {}
        }
        Ok(chunk)
    }
}
fn fixture(stdout: Vec<u8>, stderr: Vec<u8>) -> (OutputTicket, m::Receipt, Source) {
    let operation = OperationId::generate();
    let allocation = AllocationId::generate();
    let ticket = OutputTicket {
        version: 1,
        owner: OutputOwner {
            project_id: ProjectId::generate(),
            sandbox_id: SandboxId::generate(),
            operation_id: operation,
            allocation_id: allocation,
            generation: 1,
            host_id: HostId::generate(),
            host_epoch: 1,
            boot_id: "boot".into(),
        },
        upload_attempt: OperationId::generate(),
        output_limit: 10 * 1024 * 1024,
        created_unix_ms: 1000,
        expires_unix_ms: 5000,
        delete_after_unix_ms: 6000,
    };
    let stats = |b: &Vec<u8>| m::Output {
        seen: b.len() as u64,
        stored: b.len() as u64,
        truncated: false,
    };
    let receipt = m::Receipt {
        version: 1,
        context: m::Context {
            allocation_id: allocation,
            generation: 1,
            boot_id: "boot".into(),
        },
        operation_id: operation,
        digest: [0; 32],
        state: m::State::Exited,
        deadline_unix_ms: 1500,
        output_limit: ticket.output_limit,
        cancel_requested: false,
        cleanup_confirmed: true,
        exit_code: Some(0),
        signal: None,
        stdout: stats(&stdout),
        stderr: stats(&stderr),
        reason: None,
    };
    (
        ticket,
        receipt,
        Source {
            operation,
            stdout,
            stderr,
            fault: 0,
        },
    )
}
#[tokio::test]
async fn final_binary_streams_and_truncation_produce_bounded_digest_plans() {
    use sha2::{Digest, Sha256};
    let bytes = vec![0xfe; MAX_CHUNK * 3 + 1];
    let (ticket, mut receipt, source) = fixture(bytes.clone(), b"\x00\xffprivate-stderr".to_vec());
    receipt.stdout.seen += 100;
    receipt.stdout.truncated = true;
    let captured = archive::collect(&ticket, &receipt, &source).await.unwrap();
    assert_eq!(captured.stdout, bytes);
    assert_eq!(captured.stderr, source.stderr);
    assert_eq!(
        captured.plans.stdout.sha256,
        hex::encode(Sha256::digest(&bytes))
    );
    assert!(captured.plans.stdout.truncated);
    ticket.validate_plans(&captured.plans).unwrap();
    assert!(!format!("{captured:?}").contains("private-stderr"));
    let (ticket, receipt, source) = fixture(vec![], vec![]);
    assert_eq!(
        archive::collect(&ticket, &receipt, &source)
            .await
            .unwrap()
            .plans
            .stderr
            .size,
        0
    );
}
#[tokio::test]
async fn malformed_guest_chunks_are_not_published_as_successful_prefixes() {
    for fault in 1..=8 {
        let (ticket, receipt, mut source) = fixture(b"private".to_vec(), vec![]);
        source.fault = fault;
        assert_eq!(
            archive::collect(&ticket, &receipt, &source)
                .await
                .unwrap_err()
                .code(),
            Code::FailedPrecondition,
            "fault {fault}"
        );
    }
    let (ticket, receipt, mut source) = fixture(vec![], vec![]);
    source.fault = 10;
    assert_eq!(
        archive::collect(&ticket, &receipt, &source)
            .await
            .unwrap_err()
            .code(),
        Code::NotFound
    );
}
#[tokio::test(start_paused = true)]
async fn collection_has_an_overall_deadline() {
    let (ticket, receipt, mut source) = fixture(vec![0], vec![]);
    source.fault = 9;
    assert_eq!(
        archive::collect(&ticket, &receipt, &source)
            .await
            .unwrap_err()
            .code(),
        Code::Unavailable
    );
}
#[tokio::test]
async fn wrong_boot_nonfinal_receipt_and_excess_output_never_start_collection() {
    let (ticket, receipt, source) = fixture(vec![0], vec![]);
    for field in 0..5 {
        let mut r = receipt.clone();
        match field {
            0 => r.context.boot_id = "wrong".into(),
            1 => r.operation_id = OperationId::generate(),
            2 => {
                r.state = m::State::LaunchIntent;
                r.cleanup_confirmed = false;
                r.exit_code = None;
            }
            3 => r.output_limit += 1,
            _ => {
                r.stdout.stored = ticket.output_limit + 1;
                r.stdout.seen = r.stdout.stored;
            }
        }
        assert!(archive::collect(&ticket, &r, &source).await.is_err());
    }
}
#[test]
fn publication_wire_rejects_stale_oversize_or_customer_selected_metadata() {
    let (ticket, _, _) = fixture(vec![], vec![]);
    let request = OutputRequest {
        ticket_json: archive::encode(&ticket).unwrap(),
        publication_revision: 1,
        claim_expires_unix_ms: 3000,
        plans_json: vec![],
    };
    assert!(archive::decode(&request, 2000).is_ok());
    assert!(archive::decode(&request, 3000).is_err());
    for field in 0..4 {
        let mut r = request.clone();
        match field {
            0 => r.publication_revision = 0,
            1 => r.ticket_json = vec![0; archive::MAX_METADATA + 1],
            2 => r.plans_json = vec![0; archive::MAX_METADATA + 1],
            _ => {
                let mut v = serde_json::to_value(&ticket).unwrap();
                v["bucket"] = serde_json::json!("customer-selected");
                r.ticket_json = serde_json::to_vec(&v).unwrap();
            }
        }
        assert!(archive::decode(&r, 2000).is_err());
    }
}
