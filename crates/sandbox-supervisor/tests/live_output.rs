//! Validate hostile output and stale scopes without conflating temporary EOF with completion.
#![allow(clippy::unwrap_used)]
use sandbox_protocol::{
    AllocationId, HostId, Id, OperationId, ProjectId, SandboxId,
    command::CommandRecord,
    guest as w, guest_model as m,
    live_output::LiveOutputScope,
    output::OutputOwner,
    supervisor::{LiveOutputRequest, Ownership},
};
use sandbox_supervisor::live_output as live;

fn fixture() -> (
    LiveOutputScope,
    Ownership,
    CommandRecord,
    m::Receipt,
    LiveOutputRequest,
) {
    let scope = LiveOutputScope {
        version: 1,
        owner: OutputOwner {
            project_id: ProjectId::generate(),
            sandbox_id: SandboxId::generate(),
            operation_id: OperationId::generate(),
            allocation_id: AllocationId::generate(),
            host_id: HostId::generate(),
            host_epoch: 1,
            generation: 1,
            boot_id: "boot".into(),
        },
        command_digest: [3; 32],
        output_limit: 1024,
        deadline_unix_ms: 10000,
    };
    let o = &scope.owner;
    let owner = Ownership {
        project_id: o.project_id.to_string(),
        sandbox_id: o.sandbox_id.to_string(),
        operation_id: OperationId::generate().to_string(),
        allocation_id: o.allocation_id.to_string(),
        host_id: o.host_id.to_string(),
        supervisor_epoch: 1,
        generation: 1,
        claim_revision: 1,
        claim_expires_unix_ms: 1,
    };
    let receipt = m::Receipt {
        version: 1,
        context: m::Context {
            allocation_id: o.allocation_id,
            generation: 1,
            boot_id: o.boot_id.clone(),
        },
        operation_id: o.operation_id,
        digest: scope.command_digest,
        state: m::State::LaunchIntent,
        deadline_unix_ms: scope.deadline_unix_ms,
        output_limit: scope.output_limit,
        cancel_requested: false,
        cleanup_confirmed: false,
        exit_code: None,
        signal: None,
        stdout: m::Output {
            stored: 3,
            seen: 3,
            truncated: false,
        },
        stderr: m::Output::default(),
        reason: None,
    };
    let command = CommandRecord {
        digest: receipt.digest,
        context: Some(receipt.context.clone()),
        deadline_unix_ms: receipt.deadline_unix_ms,
        output_limit: receipt.output_limit,
        not_started: false,
        receipt: None,
    };
    let request = LiveOutputRequest {
        scope_json: serde_json::to_vec(&scope).unwrap(),
        output: Some(w::ReadOutput {
            operation_id: o.operation_id.to_string(),
            stream: w::Stream::Stdout as i32,
            offset: 0,
            limit: 3,
        }),
        expires_unix_ms: 2000,
    };
    (scope, owner, command, receipt, request)
}
fn chunk(r: &LiveOutputRequest) -> w::OutputChunk {
    let r = r.output.as_ref().unwrap();
    w::OutputChunk {
        operation_id: r.operation_id.clone(),
        stream: r.stream,
        offset: 0,
        data: vec![0, 255, 10],
        next_offset: 3,
        at_end: true,
        complete: false,
    }
}
#[test]
fn exact_scope_ignores_expired_mutation_claim_but_rejects_wrong_execution_binding() {
    let (s, o, c, _, _) = fixture();
    live::validate_owner(&s, &o, &c).unwrap();
    for n in 0..10 {
        let mut s = s.clone();
        match n {
            0 => s.owner.project_id = ProjectId::generate(),
            1 => s.owner.sandbox_id = SandboxId::generate(),
            2 => s.owner.allocation_id = AllocationId::generate(),
            3 => s.owner.host_id = HostId::generate(),
            4 => s.owner.host_epoch += 1,
            5 => s.owner.generation += 1,
            6 => s.owner.boot_id.push('x'),
            7 => s.command_digest[0] ^= 1,
            8 => s.output_limit += 1,
            _ => s.deadline_unix_ms += 1,
        }
        assert!(live::validate_owner(&s, &o, &c).is_err(), "field {n}");
    }
    assert!(live::validate_owner(&s, &o, &CommandRecord::fenced(s.command_digest)).is_err());
}
#[test]
fn reads_require_bounded_fresh_exact_requests() {
    let (_, _, _, _, r) = fixture();
    live::decode(&r, 1000).unwrap();
    for n in 0..11 {
        let mut r = r.clone();
        match n {
            0 => r.expires_unix_ms = 1000,
            1 => r.expires_unix_ms = 31001,
            2 => r.scope_json = vec![b' '; 8193],
            3 => r.output = None,
            4 => r.output.as_mut().unwrap().limit = 0,
            5 => r.output.as_mut().unwrap().limit = 32769,
            6 => r.output.as_mut().unwrap().offset = 1025,
            7 => r.output.as_mut().unwrap().stream = 99,
            8 => r.output.as_mut().unwrap().operation_id = OperationId::generate().to_string(),
            9 => {
                let mut v: serde_json::Value = serde_json::from_slice(&r.scope_json).unwrap();
                v["extra"] = true.into();
                r.scope_json = serde_json::to_vec(&v).unwrap();
            }
            _ => r.scope_json = b"{}".to_vec(),
        }
        assert!(live::decode(&r, 1000).is_err(), "case {n}");
    }
}
#[test]
fn binary_partial_and_final_reads_preserve_completion_races() {
    let (s, _, c, mut receipt, r) = fixture();
    let read = r.output.as_ref().unwrap();
    let mut ch = chunk(&r);
    live::validate_chunk(&s, read, &c, &ch, &receipt).unwrap();
    ch.complete = true;
    assert!(live::validate_chunk(&s, read, &c, &ch, &receipt).is_err());
    receipt.state = m::State::Exited;
    receipt.cleanup_confirmed = true;
    receipt.exit_code = Some(0);
    live::validate_chunk(&s, read, &c, &ch, &receipt).unwrap();
    // The process may finish between reading bytes and inspecting the receipt.
    ch.complete = false;
    receipt.stdout.stored = 4;
    receipt.stdout.seen = 4;
    live::validate_chunk(&s, read, &c, &ch, &receipt).unwrap();
    ch.complete = true;
    assert!(live::validate_chunk(&s, read, &c, &ch, &receipt).is_err());
    ch.at_end = false;
    live::validate_chunk(&s, read, &c, &ch, &receipt).unwrap();
}
#[test]
fn rejects_invalid_bytes_bounds_receipts_and_unknown_history() {
    let (s, _, c, receipt, r) = fixture();
    let read = r.output.as_ref().unwrap();
    for n in 0..12 {
        let mut ch = chunk(&r);
        let mut receipt = receipt.clone();
        match n {
            0 => ch.operation_id = OperationId::generate().to_string(),
            1 => ch.stream = w::Stream::Stderr as i32,
            2 => ch.offset = 1,
            3 => ch.next_offset = 4,
            4 => ch.data.push(4),
            5 => ch.at_end = false,
            6 => {
                ch.data.clear();
                ch.next_offset = 0;
                ch.at_end = false;
            }
            7 => receipt.state = m::State::Unknown,
            8 => receipt.digest[0] ^= 1,
            9 => receipt.context.boot_id.push('x'),
            10 => receipt.stdout.stored = 2,
            _ => {
                ch.offset = u64::MAX;
                ch.next_offset = 2;
            }
        }
        assert!(
            live::validate_chunk(&s, read, &c, &ch, &receipt).is_err(),
            "case {n}"
        );
    }
}
#[test]
fn empty_stderr_can_be_pending_or_complete_and_debug_redacts_payload() {
    let (s, _, c, mut receipt, mut r) = fixture();
    r.output.as_mut().unwrap().stream = w::Stream::Stderr as i32;
    let mut ch = chunk(&r);
    ch.data.clear();
    ch.next_offset = 0;
    live::validate_chunk(&s, r.output.as_ref().unwrap(), &c, &ch, &receipt).unwrap();
    receipt.state = m::State::Exited;
    receipt.cleanup_confirmed = true;
    receipt.exit_code = Some(0);
    ch.complete = true;
    live::validate_chunk(&s, r.output.as_ref().unwrap(), &c, &ch, &receipt).unwrap();
    receipt.reason = Some("secret-output-content".into());
    ch.data = b"secret-output-content".to_vec();
    let observation = live::observation(
        r.clone(),
        s.owner.host_id.to_string(),
        1,
        false,
        1000,
        ch,
        receipt,
    )
    .unwrap();
    assert!(!format!("{observation:?}").contains("secret-output-content"));
    r.scope_json = b"secret-output-content".to_vec();
    assert!(!format!("{r:?}").contains("secret-output-content"));
}
