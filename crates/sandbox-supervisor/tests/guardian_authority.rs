//! Controlled Linux/KVM evidence for guardian authority enforcement.
#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used)]
#[path = "support/vm.rs"]
mod vm;
use sandbox_protocol::{
    AllocationId, Id, OperationId, ProjectId, SandboxId, allocation_authority::Permit,
};
use sandbox_supervisor::{
    guardian::{self, Action},
    launch_authority::AuthorityFile,
};
use std::fs;

fn permit(f: &vm::Fixture, serial: u64) -> Permit {
    let o = &f.manifest.start.owner;
    Permit {
        host: o.host,
        project: o.project,
        sandbox: o.sandbox,
        allocation: o.allocation,
        create_operation: o.create_operation,
        generation: o.generation,
        original_epoch: o.epoch,
        serial,
    }
}
fn save(f: &vm::Fixture) {
    fs::write(&f.path, serde_json::to_vec(&f.manifest).unwrap()).unwrap();
}
#[test]
#[ignore = "requires root, HUDSON_GUARDIAN_TEST_VM=1 and controlled Firecracker artifacts"]
fn real_guardian_fences_launch_renewal_and_replay_after_metadata_removal() {
    let mut f = vm::Fixture::new(20000);
    let store = AuthorityFile::initialize(
        f.manifest.config.state_root.clone(),
        f.manifest.start.owner.host,
        1,
    )
    .unwrap();
    let p = permit(&f, 1);
    store.register(1, std::slice::from_ref(&p)).unwrap();
    f.manifest.launch_permit = Some(p.clone());
    save(&f);
    let mut wrong = f.manifest.clone();
    wrong.start.owner.project = ProjectId::generate();
    assert!(wrong.prepare().is_err());
    let mut child = f.spawn();
    f.running(&mut child);
    assert!(f.populated());
    let retirement = OperationId::generate();
    // Success while the VM is running proves namespace init released the
    // global shared lock after its launch decision, not at VM shutdown.
    store.fence(1, &p, retirement).unwrap();
    assert!(
        guardian::control(
            &f.manifest,
            Action::Renew {
                revision: 1,
                expires_unix_ms: guardian::wall_ms() + 30000
            }
        )
        .unwrap()
        .error
        .is_some()
    );
    assert!(
        guardian::control(&f.manifest, Action::Inspect)
            .unwrap()
            .error
            .is_none()
    );
    assert!(f.populated(), "a fence is not physical cleanup evidence");
    assert!(
        guardian::control(&f.manifest, Action::Stop)
            .unwrap()
            .error
            .is_none()
    );
    assert!(child.wait().unwrap().success());
    f.stopped();
    // Model the future trusted cleanup coordinator only after real stop and
    // cgroup/runtime cleanup. This test does not claim a database ack protocol.
    fs::remove_dir_all(f.manifest.directory()).unwrap();
    fs::File::open(&f.manifest.config.state_root)
        .unwrap()
        .sync_all()
        .unwrap();
    let scope = sandbox_protocol::allocation_retirement::Intent {
        version: 1,
        retirement,
        permit: p.clone(),
        commands: sandbox_protocol::allocation_retirement::DomainClosure::Empty {},
        files: sandbox_protocol::allocation_retirement::DomainClosure::Empty {},
        // Synthetic database binding in this component test, not a DB receipt.
        release_evidence_sha256: "a".repeat(64),
        simulated: false,
    };
    store.complete(1, &scope).unwrap();
    store.forget(1, &scope).unwrap();
    assert!(f.manifest.prepare().is_err());
    assert!(!f.manifest.directory().exists());
    let mut legacy = f.manifest.clone();
    legacy.launch_permit = None;
    assert!(legacy.prepare().is_err());
    assert!(!f.manifest.directory().exists());
    assert!(f.manifest.config.state_root.join("launch.lock").exists());

    // A new allocation can use the same root. Advancing the persisted epoch
    // subsequently rejects its old manifest and renewal independently of RPC.
    store.advance_epoch(1, 2).unwrap();
    f.manifest.start.owner.allocation = AllocationId::generate();
    f.manifest.start.owner.sandbox = SandboxId::generate();
    f.manifest.start.owner.create_operation = OperationId::generate();
    f.manifest.start.owner.epoch = 2;
    f.manifest.start.expires_unix_ms = guardian::wall_ms() + 20000;
    let p2 = permit(&f, 2);
    store.register(2, std::slice::from_ref(&p2)).unwrap();
    f.manifest.launch_permit = Some(p2);
    save(&f);
    let mut child = f.spawn();
    f.running(&mut child);
    store.advance_epoch(2, 3).unwrap();
    assert!(f.manifest.prepare().is_err());
    assert!(
        guardian::control(
            &f.manifest,
            Action::Renew {
                revision: 1,
                expires_unix_ms: guardian::wall_ms() + 30000
            }
        )
        .unwrap()
        .error
        .is_some()
    );
    assert!(
        guardian::control(&f.manifest, Action::Stop)
            .unwrap()
            .error
            .is_none()
    );
    assert!(child.wait().unwrap().success());
    f.stopped();
}
