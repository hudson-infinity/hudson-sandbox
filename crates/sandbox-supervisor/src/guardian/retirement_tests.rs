#![allow(clippy::unwrap_used)]
use super::*;
use crate::launch_authority::AuthorityFile;
use sandbox_protocol::{allocation_authority::Permit, allocation_retirement::DomainClosure};
use std::os::unix::fs::symlink;

fn fixture() -> (tempfile::TempDir, AuthorityFile, Manifest, Intent) {
    assert_eq!(std::env::var("HUDSON_GUARDIAN_TEST_VM").as_deref(), Ok("1"));
    let temp = tempfile::tempdir().unwrap();
    let host = HostId::generate();
    let root = temp.path().join("a");
    let authority = AuthorityFile::initialize(root.clone(), host, 1).unwrap();
    let p = Permit {
        host,
        project: ProjectId::generate(),
        sandbox: SandboxId::generate(),
        allocation: AllocationId::generate(),
        create_operation: OperationId::generate(),
        generation: 1,
        original_epoch: 1,
        serial: 1,
    };
    authority.register(1, std::slice::from_ref(&p)).unwrap();
    let artifact = Artifact {
        path: "/unused".into(),
        sha256: "ab".repeat(32),
    };
    let manifest = Manifest {
        launch_permit: Some(p.clone()),
        config: Config {
            state_root: root,
            cgroup_parent: "/sys/fs/cgroup/hudson-guardians-tests".into(),
            firecracker: artifact.clone(),
            jailer: artifact.clone(),
            kernel: artifact.clone(),
            rootfs: artifact,
            jail_uid: 65534,
            jail_gid: 65534,
        },
        start: Start {
            owner: Owner {
                host,
                project: p.project,
                sandbox: p.sandbox,
                allocation: p.allocation,
                create_operation: p.create_operation,
                generation: 1,
                epoch: 1,
            },
            vcpu: 1,
            memory_mib: 128,
            disk_mib: 64,
            expires_unix_ms: wall_ms() + 30000,
        },
    };
    let intent = Intent {
        version: 1,
        retirement: OperationId::generate(),
        permit: p,
        commands: DomainClosure::Empty {},
        files: DomainClosure::Empty {},
        release_evidence_sha256: "cd".repeat(32),
        simulated: false,
    };
    (temp, authority, manifest, intent)
}
fn open(m: &Manifest, i: &Intent, plan: Option<&Plan>) -> Result<Session> {
    Session::open(
        &m.config.state_root,
        &m.config.cgroup_parent,
        1,
        1,
        i,
        Some(m),
        plan,
    )
}
#[test]
#[ignore = "requires root in the dedicated HUDSON_GUARDIAN_TEST_VM"]
fn each_owned_unlink_can_resume_and_delayed_cleanup_cannot_recreate_metadata() {
    let (temp, authority, m, intent) = fixture();
    m.fence_unstarted().unwrap();
    authority
        .fence(1, &intent.permit, intent.retirement)
        .unwrap();
    let old_lock = File::open(m.directory().join("lifecycle.lock")).unwrap();
    let mut session = open(&m, &intent, None).unwrap();
    let plan_path = temp.path().join("retained-plan.json");
    write_json(&plan_path, &session.plan).unwrap();
    assert!(m.reconcile("delayed").is_err());
    assert!(m.fence_unstarted().is_err());
    for remaining in [2, 1, 0] {
        assert!(!session.remove_next().unwrap());
        drop(session);
        let names: Vec<_> = fs::read_dir(m.directory())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names.len(), remaining);
        assert!(m.reconcile("delayed").is_err());
        assert!(m.fence_unstarted().is_err());
        assert_eq!(fs::read_dir(m.directory()).unwrap().count(), remaining);
        let plan: Plan = read_json(&plan_path).unwrap();
        session = open(&m, &intent, Some(&plan)).unwrap();
    }
    assert!(session.remove_next().unwrap());
    drop(session);
    assert!(!m.directory().exists());
    // A stale inode can become lockable, but never grants launch/metadata ownership.
    rustix::fs::flock(
        &old_lock,
        rustix::fs::FlockOperation::NonBlockingLockExclusive,
    )
    .unwrap();
    assert!(m.prepare().is_err());
    assert!(m.reconcile("delayed").is_err());
    assert!(m.fence_unstarted().is_err());
    assert!(!m.directory().exists());
    let plan: Plan = read_json(&plan_path).unwrap();
    let mut resumed = open(&m, &intent, Some(&plan)).unwrap();
    assert!(resumed.is_removed());
    assert!(resumed.remove_next().unwrap());
    drop(resumed);
    assert!(authority.checkpoint(1).is_ok());
    assert!(
        crate::launch_authority::authorize(&m.config.state_root, Some(&intent.permit)).is_err()
    );
}
#[test]
#[ignore = "requires root in the dedicated HUDSON_GUARDIAN_TEST_VM"]
fn deletion_rejects_live_locks_unowned_entries_missing_evidence_and_scope_changes() {
    let (_temp, authority, m, intent) = fixture();
    m.fence_unstarted().unwrap();
    assert!(open(&m, &intent, None).is_err()); // Active is not launch denial.
    authority
        .fence(1, &intent.permit, intent.retirement)
        .unwrap();
    fs::create_dir_all(&m.config.cgroup_parent).unwrap();
    fs::create_dir(m.group()).unwrap();
    assert!(open(&m, &intent, None).is_err());
    assert!(m.group().exists());
    fs::remove_dir(m.group()).unwrap();
    let lock = m.lifecycle_lock(false).unwrap();
    assert!(open(&m, &intent, None).is_err());
    drop(lock);
    fs::write(m.directory().join("unowned"), b"keep").unwrap();
    assert!(open(&m, &intent, None).is_err());
    assert_eq!(fs::read(m.directory().join("unowned")).unwrap(), b"keep");
    fs::remove_file(m.directory().join("unowned")).unwrap();
    let session = open(&m, &intent, None).unwrap();
    let plan = session.plan.clone();
    drop(session);
    let mut corrupt = plan.clone();
    if let Metadata::Stopped { receipt, .. } = &mut corrupt.metadata {
        receipt.reason = Some("changed_after_removal".into());
    }
    assert!(open(&m, &intent, Some(&corrupt)).is_err());
    let mut changed = intent.clone();
    changed.release_evidence_sha256 = "ef".repeat(32);
    assert!(open(&m, &changed, Some(&plan)).is_err());
    changed = intent.clone();
    changed.retirement = OperationId::generate();
    assert!(open(&m, &changed, Some(&plan)).is_err());
    changed = intent.clone();
    changed.simulated = true;
    assert!(open(&m, &changed, Some(&plan)).is_err());
    let receipt = m.record_path();
    let bytes = fs::read(&receipt).unwrap();
    fs::write(&receipt, b"{}").unwrap();
    assert!(open(&m, &intent, Some(&plan)).is_err());
    fs::write(&receipt, bytes).unwrap();
    let alias = m.directory().join("alias");
    fs::hard_link(&receipt, &alias).unwrap();
    assert!(open(&m, &intent, Some(&plan)).is_err());
    fs::remove_file(alias).unwrap();
    fs::remove_file(&receipt).unwrap();
    symlink("manifest.json", &receipt).unwrap();
    assert!(open(&m, &intent, Some(&plan)).is_err());
    fs::remove_file(receipt).unwrap();
    assert!(open(&m, &intent, None).is_err()); // Missing alone is never cleanup.
    assert!(open(&m, &intent, Some(&plan)).is_ok()); // Retained deletion intent permits partial removal.
}
#[test]
#[ignore = "requires root in the dedicated HUDSON_GUARDIAN_TEST_VM"]
fn unused_permit_requires_absence_and_cannot_consume_a_guardian_plan() {
    let (_temp, authority, m, intent) = fixture();
    authority
        .fence(1, &intent.permit, intent.retirement)
        .unwrap();
    let mut session = Session::open(
        &m.config.state_root,
        &m.config.cgroup_parent,
        1,
        1,
        &intent,
        None,
        None,
    )
    .unwrap();
    let plan = session.plan.clone();
    assert!(session.remove_next().unwrap());
    drop(session);
    assert!(open(&m, &intent, Some(&plan)).is_err());
    fs::create_dir(m.directory()).unwrap();
    assert!(
        Session::open(
            &m.config.state_root,
            &m.config.cgroup_parent,
            1,
            1,
            &intent,
            None,
            Some(&plan)
        )
        .is_err()
    );
    assert!(m.fence_unstarted().is_err());
    assert_eq!(fs::read_dir(m.directory()).unwrap().count(), 0);
}
