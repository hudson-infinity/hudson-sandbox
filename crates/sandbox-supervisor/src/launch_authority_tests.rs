#![allow(clippy::unwrap_used)]
use super::*;
use sandbox_protocol::{AllocationId, Id, ProjectId, SandboxId};
use std::os::unix::fs::{PermissionsExt, symlink};
fn setup() -> (tempfile::TempDir, AuthorityFile, Permit) {
    assert_eq!(std::env::var("HUDSON_GUARDIAN_TEST_VM").as_deref(), Ok("1"));
    assert!(rustix::process::geteuid().is_root());
    let dir = tempfile::tempdir().unwrap();
    let host = HostId::generate();
    let store = AuthorityFile::initialize(dir.path().join("a"), host, 1).unwrap();
    let permit = Permit {
        host,
        project: ProjectId::generate(),
        sandbox: SandboxId::generate(),
        allocation: AllocationId::generate(),
        create_operation: OperationId::generate(),
        generation: 1,
        original_epoch: 1,
        serial: 1,
    };
    store.register(1, std::slice::from_ref(&permit)).unwrap();
    (dir, store, permit)
}
#[test]
#[ignore = "requires root in the dedicated HUDSON_GUARDIAN_TEST_VM"]
fn epochs_and_retirement_survive_reopen_without_reauthorizing_old_permits() {
    let (_dir, store, p) = setup();
    let guard = authorize(&store.root, Some(&p)).unwrap();
    assert!(store.advance_epoch(1, 2).is_err());
    drop(guard);
    store.advance_epoch(1, 2).unwrap();
    assert!(authorize(&store.root, Some(&p)).is_err());
    assert!(store.advance_epoch(2, 1).is_err());
    let mut next = p.clone();
    next.allocation = AllocationId::generate();
    next.sandbox = SandboxId::generate();
    next.create_operation = OperationId::generate();
    next.serial = 2;
    next.original_epoch = 2;
    assert!(store.register(1, std::slice::from_ref(&next)).is_err());
    store.register(2, std::slice::from_ref(&next)).unwrap();
    drop(authorize(&store.root, Some(&next)).unwrap());
    let reopened = AuthorityFile::open(store.root.clone(), p.host, 2, 2).unwrap();
    let retirement = OperationId::generate();
    reopened.fence(2, &p, retirement).unwrap();
    reopened.complete(2, &p, retirement).unwrap();
    reopened.forget(2, &p, retirement).unwrap();
    assert!(authorize(&store.root, Some(&p)).is_err());
    drop(authorize(&store.root, Some(&next)).unwrap());
    assert!(reopened.register(2, &[p]).is_err());
    assert!(AuthorityFile::open(store.root.clone(), store.host, 3, 2).is_err());
    assert!(AuthorityFile::open(store.root.clone(), store.host, 2, 3).is_err());
}
#[test]
#[ignore = "requires root in the dedicated HUDSON_GUARDIAN_TEST_VM"]
fn cross_process_gate_survives_allocation_directory_deletion_and_detects_replaced_lock() {
    let (_dir, store, p) = setup();
    let guard = authorize(&store.root, Some(&p)).unwrap();
    let result=std::process::Command::new("/usr/bin/python3").args(["-c",
        "import fcntl,sys\nf=open(sys.argv[1],'rb')\ntry: fcntl.flock(f,fcntl.LOCK_EX|fcntl.LOCK_NB)\nexcept BlockingIOError: sys.exit(0)\nsys.exit(1)"])
        .arg(store.root.join(LOCK)).status().unwrap();
    assert!(result.success());
    assert!(store.fence(1, &p, OperationId::generate()).is_err());
    drop(guard);
    let allocation = store.root.join(p.allocation.to_string());
    fs::create_dir(&allocation).unwrap();
    let retirement = OperationId::generate();
    store.fence(1, &p, retirement).unwrap();
    fs::remove_dir(&allocation).unwrap(); // controlled empty metadata fixture
    store.complete(1, &p, retirement).unwrap();
    store.forget(1, &p, retirement).unwrap();
    assert!(authorize(&store.root, Some(&p)).is_err());
    assert!(authorize(&store.root, None).is_err());
    let original = File::open(store.root.join(LOCK)).unwrap();
    fs::remove_file(store.root.join(LOCK)).unwrap();
    assert!(AuthorityFile::open(store.root.clone(), store.host, 1, 1).is_err());
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(store.root.join(LOCK))
        .unwrap();
    assert!(AuthorityFile::open(store.root.clone(), store.host, 1, 1).is_err());
    drop(original);
}
#[test]
#[ignore = "requires root in the dedicated HUDSON_GUARDIAN_TEST_VM"]
fn missing_corrupt_linked_or_rolled_back_files_never_initialize_empty_authority() {
    let (_dir, store, p) = setup();
    let path = store.root.join(STATE);
    let bytes = fs::read(&path).unwrap();
    for data in [
        b"".to_vec(),
        b"{}".to_vec(),
        vec![b'x'; MAX_BYTES as usize + 1],
    ] {
        fs::write(&path, data).unwrap();
        assert!(authorize(&store.root, Some(&p)).is_err());
        assert!(authorize(&store.root, None).is_err());
        assert!(AuthorityFile::initialize(store.root.clone(), store.host, 1).is_err());
    }
    fs::write(&path, &bytes).unwrap();
    let mut value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    value["unknown"] = true.into();
    fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
    assert!(authorize(&store.root, Some(&p)).is_err());
    fs::write(&path, &bytes).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(authorize(&store.root, Some(&p)).is_err());
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    let other = store.root.join("other");
    fs::hard_link(&path, &other).unwrap();
    assert!(authorize(&store.root, Some(&p)).is_err());
    fs::remove_file(&other).unwrap();
    fs::rename(&path, &other).unwrap();
    symlink(&other, &path).unwrap();
    assert!(authorize(&store.root, Some(&p)).is_err());
    fs::remove_file(&path).unwrap();
    assert!(authorize(&store.root, None).is_err());
    fs::rename(&other, &path).unwrap();
    store.advance_epoch(1, 2).unwrap();
    fs::write(&path, bytes).unwrap(); // independently supplied checkpoint detects rollback
    assert!(AuthorityFile::open(store.root.clone(), store.host, 2, 1).is_err());
}
#[test]
#[ignore = "requires root in the dedicated HUDSON_GUARDIAN_TEST_VM"]
fn failed_save_retains_original_state_and_stale_staging_is_bounded() {
    let (_dir, store, p) = setup();
    let before = fs::read(store.root.join(STATE)).unwrap();
    fs::create_dir(store.root.join(NEXT)).unwrap();
    let retirement = OperationId::generate();
    assert!(store.fence(1, &p, retirement).is_err());
    assert_eq!(fs::read(store.root.join(STATE)).unwrap(), before);
    // A failed fence is not completion: callers must retain the allocation.
    drop(authorize(&store.root, Some(&p)).unwrap());
    fs::remove_dir(store.root.join(NEXT)).unwrap();
    let mut stale = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(store.root.join(NEXT))
        .unwrap();
    stale.write_all(b"interrupted uncommitted write").unwrap();
    drop(stale);
    store.fence(1, &p, retirement).unwrap();
    assert!(!store.root.join(NEXT).exists());
    assert!(authorize(&store.root, Some(&p)).is_err());
    assert_eq!(fs::read_dir(&store.root).unwrap().count(), 3);
}

#[test]
#[ignore = "requires root in the dedicated HUDSON_GUARDIAN_TEST_VM"]
fn activation_cannot_race_a_legacy_prepare_or_adopt_existing_allocation_metadata() {
    assert_eq!(std::env::var("HUDSON_GUARDIAN_TEST_VM").as_deref(), Ok("1"));
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("a");
    let host = HostId::generate();
    let guard = authorize(&root, None).unwrap();
    assert!(AuthorityFile::initialize(root.clone(), host, 1).is_err());
    fs::create_dir(root.join("legacy-allocation")).unwrap();
    drop(guard);
    assert!(AuthorityFile::initialize(root.clone(), host, 1).is_err());
    assert!(!root.join(REQUIRED).exists());
    fs::remove_dir(root.join("legacy-allocation")).unwrap();
    AuthorityFile::initialize(root.clone(), host, 1).unwrap();
    assert!(authorize(&root, None).is_err());
    fs::remove_file(root.join(STATE)).unwrap();
    assert!(authorize(&root, None).is_err());
    assert!(AuthorityFile::initialize(root, host, 1).is_err());
}

#[test]
#[ignore = "requires root in the dedicated HUDSON_GUARDIAN_TEST_VM"]
fn orphan_staging_is_not_a_legacy_root() {
    let (_dir, store, _p) = setup();
    fs::rename(store.root.join(STATE), store.root.join(NEXT)).unwrap();
    fs::remove_file(store.root.join(REQUIRED)).unwrap();
    assert!(authorize(&store.root, None).is_err());
    assert!(AuthorityFile::initialize(store.root.clone(), store.host, 1).is_err());
    fs::remove_file(store.root.join(LOCK)).unwrap();
    assert!(authorize(&store.root, None).is_err());
    assert!(!store.root.join(LOCK).exists());
}

#[test]
#[ignore = "requires root in the dedicated HUDSON_GUARDIAN_TEST_VM"]
fn registration_ack_reconciles_without_regranting_forgotten_or_fenced_owners() {
    let (_dir, store, first) = setup();
    let expected = Checkpoint {
        host: first.host,
        epoch: 1,
        registered_through: 1,
    };
    assert_eq!(store.checkpoint(1).unwrap(), expected);
    assert_eq!(
        store.register(1, std::slice::from_ref(&first)).unwrap(),
        expected
    );
    let retirement = OperationId::generate();
    store.fence(1, &first, retirement).unwrap();
    assert_eq!(
        store.register(1, std::slice::from_ref(&first)).unwrap(),
        expected
    );
    assert!(authorize(&store.root, Some(&first)).is_err());
    store.complete(1, &first, retirement).unwrap();
    store.forget(1, &first, retirement).unwrap();
    let reopened = AuthorityFile::open(store.root.clone(), first.host, 1, 1).unwrap();
    assert_eq!(reopened.checkpoint(1).unwrap(), expected);
    assert!(reopened.register(1, std::slice::from_ref(&first)).is_err());
    assert!(authorize(&store.root, Some(&first)).is_err());
    let mut second = first.clone();
    second.serial = 2;
    second.allocation = AllocationId::generate();
    second.sandbox = SandboxId::generate();
    second.create_operation = OperationId::generate();
    fs::create_dir(store.root.join(NEXT)).unwrap();
    assert!(reopened.register(1, std::slice::from_ref(&second)).is_err());
    assert_eq!(reopened.checkpoint(1).unwrap(), expected);
    assert!(authorize(&store.root, Some(&second)).is_err());
    fs::remove_dir(store.root.join(NEXT)).unwrap();
    let ack = reopened.register(1, std::slice::from_ref(&second)).unwrap();
    assert_eq!(ack.registered_through, 2);
    assert_eq!(reopened.checkpoint(1).unwrap(), ack);
    reopened.advance_epoch(1, 2).unwrap();
    assert!(reopened.checkpoint(1).is_err());
    assert_eq!(reopened.checkpoint(2).unwrap().registered_through, 2);
    fs::remove_file(store.root.join(STATE)).unwrap();
    assert!(reopened.checkpoint(2).is_err());
}

#[test]
#[ignore = "requires root in the dedicated HUDSON_GUARDIAN_TEST_VM"]
fn receipt_admission_holds_persistent_gate_and_checks_epoch_and_frontier() {
    let (_dir, store, p) = setup();
    let (owner, guard) =
        authorize_registered_allocation(&store.root, p.host, 1, 1, p.allocation).unwrap();
    assert_eq!(owner, p);
    assert!(store.fence(1, &p, OperationId::generate()).is_err());
    drop(guard);
    assert!(authorize_registered_allocation(&store.root, p.host, 1, 2, p.allocation).is_err());
    assert!(
        authorize_registered_allocation(&store.root, HostId::generate(), 1, 1, p.allocation)
            .is_err()
    );
    store.advance_epoch(1, 2).unwrap();
    assert!(authorize_registered_allocation(&store.root, p.host, 1, 1, p.allocation).is_err());
    assert!(authorize_registered_allocation(&store.root, p.host, 2, 1, p.allocation).is_err());
}
