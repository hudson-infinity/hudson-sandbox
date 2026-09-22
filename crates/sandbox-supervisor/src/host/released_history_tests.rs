#![allow(clippy::unwrap_used)]
use super::*;
use sandbox_protocol::{
    command::CommandRecord, guest_model::Context, supervisor_files::FileRecord,
};
fn fixture() -> (Record, sandbox_protocol::history::Barrier) {
    let context = Context {
        allocation_id: AllocationId::generate(),
        generation: 1,
        boot_id: "boot".into(),
    };
    let owner = Ownership {
        host_id: HostId::generate().to_string(),
        project_id: ProjectId::generate().to_string(),
        sandbox_id: SandboxId::generate().to_string(),
        allocation_id: context.allocation_id.to_string(),
        operation_id: OperationId::generate().to_string(),
        generation: 1,
        supervisor_epoch: 1,
        claim_revision: 1,
        claim_expires_unix_ms: 1,
    };
    let record = Record {
        owner,
        revisions: BTreeMap::new(),
        create: None,
        manifest: None,
        dispatched: false,
        stopped: false,
        released: false,
        commands: BTreeMap::new(),
        files: BTreeMap::new(),
        archives: BTreeMap::new(),
        command_history: None,
        released_commands: None,
        released_files: None,
        file_history: None,
        lease_revision: 0,
        lease_request: None,
        gate: Arc::new(Mutex::new(())),
        file_io: journal::file_io(),
    };
    (
        record,
        sandbox_protocol::history::Barrier {
            version: 1,
            context,
            domain: Domain::Commands,
            through: id(100),
        },
    )
}
fn id(n: u64) -> OperationId {
    format!("op_019a9fad-3000-7000-8000-{n:012x}")
        .parse()
        .unwrap()
}

#[test]
fn destruction_reclaims_only_known_prefix_and_preserves_allocation_tombstone() {
    let (mut r, _) = fixture();
    r.stopped = true;
    r.commands
        .insert(id(1).to_string(), CommandRecord::fenced([1; 32]));
    r.commands
        .insert(id(2).to_string(), CommandRecord::fenced([2; 32]));
    r.files
        .insert(id(1).to_string(), FileRecord::fenced([3; 32]));
    r.revisions.insert(id(1).to_string(), 1);
    let lifecycle = r.owner.operation_id.clone();
    r.revisions.insert(lifecycle.clone(), 1);
    assert_eq!(
        reclaim(&mut r, Domain::Commands, id(1), 1, 1).unwrap(),
        id(1)
    );
    assert!(!r.commands.contains_key(&id(1).to_string()));
    assert!(r.commands.contains_key(&id(2).to_string()));
    assert!(r.files.contains_key(&id(1).to_string()));
    assert!(r.revisions.contains_key(&lifecycle));
    assert!(r.stopped);
    assert!(r.manifest.is_none()); // No fabricated guest boot for fenced absence.
    assert!(check(&r, Domain::Commands, id(1)).is_err());
    assert!(check(&r, Domain::Files, id(1)).is_ok());
    let mut restored: Record = serde_json::from_slice(&serde_json::to_vec(&r).unwrap()).unwrap();
    validate_retained(&restored, 2).unwrap();
    assert_eq!(
        reclaim(&mut restored, Domain::Commands, id(1), 2, 2).unwrap(),
        id(1)
    );
    assert_eq!(
        reclaim(&mut restored, Domain::Commands, id(2), 3, 2).unwrap(),
        id(2)
    );
    assert_eq!(
        reclaim(&mut restored, Domain::Commands, id(1), 4, 2).unwrap(),
        id(2)
    );
    assert!(reclaim(&mut restored, Domain::Commands, id(2), 4, 2).is_err());
    assert!(reclaim(&mut restored, Domain::Commands, id(2), 3, 2).is_err());
    assert!(reclaim(&mut restored, Domain::Commands, id(2), 5, 1).is_err());
    assert!(restored.commands.is_empty());
}
#[test]
fn unfenced_owner_and_corrupt_retained_proof_fail_closed() {
    let (mut r, _) = fixture();
    assert!(reclaim(&mut r, Domain::Files, id(1), 1, 1).is_err());
    r.stopped = true;
    reclaim(&mut r, Domain::Files, id(1), 1, 1).unwrap();
    r.files
        .insert(id(1).to_string(), FileRecord::fenced([1; 32]));
    assert!(validate_retained(&r, 1).is_err());
    r.files.clear();
    r.stopped = false;
    assert!(validate_retained(&r, 1).is_err());
    r.stopped = true;
    r.released_files.as_mut().unwrap().version = 2;
    assert!(validate_retained(&r, 1).is_err());
    r.released_files.as_mut().unwrap().version = 1;
    assert!(validate_retained(&r, 0).is_err());
}
#[test]
fn unknown_command_blocks_prefix_even_after_destruction() {
    let (mut r, barrier) = fixture();
    r.stopped = true;
    let pending = CommandRecord {
        digest: [1; 32],
        context: Some(barrier.context),
        deadline_unix_ms: 10,
        output_limit: 1,
        not_started: false,
        receipt: None,
    };
    r.commands.insert(id(1).to_string(), pending);
    r.commands
        .insert(id(2).to_string(), CommandRecord::fenced([2; 32]));
    let before = serde_json::to_value(&r).unwrap();
    assert!(reclaim(&mut r, Domain::Commands, id(2), 1, 1).is_err());
    assert_eq!(serde_json::to_value(&r).unwrap(), before);
}

#[test]
fn unfinished_and_unknown_uploads_block_retirement_without_changing_state() {
    for state in [0, 1, 2, 4] {
        let (mut r, barrier) = fixture();
        r.stopped = true;
        r.files.insert(
            id(1).to_string(),
            FileRecord {
                digest: [1; 32],
                context: Some(barrier.context),
                size: 1,
                not_started: false,
                commit_requested: state == 2,
                abort_requested: false,
                state,
            },
        );
        let before = serde_json::to_value(&r).unwrap();
        assert!(reclaim(&mut r, Domain::Files, id(1), 1, 1).is_err());
        assert_eq!(serde_json::to_value(&r).unwrap(), before);
    }
}
