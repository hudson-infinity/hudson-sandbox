//! Unprivileged Linux filesystem tests; no KVM, root, cgroups or host services.
#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used)]
use sandbox_guest::files::Transfers;
use sandbox_protocol::{AllocationId, Id, OperationId, files::*, guest_model::Context};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::Write,
    os::unix::fs::{PermissionsExt, symlink},
};

fn context() -> Context {
    Context {
        allocation_id: AllocationId::generate(),
        generation: 1,
        boot_id: "test-boot".into(),
    }
}
fn upload(path: &str, data: &[u8]) -> Upload {
    Upload {
        operation_id: OperationId::generate(),
        path: path.into(),
        size: data.len() as u64,
        sha256: Sha256::digest(data).into(),
        mode: 0o644,
    }
}
fn fill(engine: &mut Transfers, request: &Upload, data: &[u8]) {
    engine.begin(request.clone()).unwrap();
    for (index, chunk) in data.chunks(MAX_CHUNK_BYTES).enumerate() {
        engine
            .write_chunk(
                request.operation_id,
                (index * MAX_CHUNK_BYTES) as u64,
                chunk,
            )
            .unwrap();
    }
}
#[test]
fn upload_is_atomic_and_completed_retries_do_not_overwrite_later_work() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join("dir")).unwrap();
    fs::write(dir.path().join("dir/file"), b"old").unwrap();
    let ctx = context();
    let mut engine = Transfers::open(dir.path(), ctx.clone()).unwrap();
    let data = vec![7; MAX_CHUNK_BYTES * 2 + 19];
    let mut request = upload("dir/file", &data);
    request.mode = 0o755;
    fill(&mut engine, &request, &data);
    assert_eq!(fs::read(dir.path().join("dir/file")).unwrap(), b"old");
    assert_eq!(
        engine.commit(request.operation_id).unwrap().state,
        State::Committed
    );
    assert_eq!(fs::read(dir.path().join("dir/file")).unwrap(), data);
    assert_eq!(
        fs::metadata(dir.path().join("dir/file"))
            .unwrap()
            .permissions()
            .mode()
            & 0o7777,
        0o755
    );
    fs::write(dir.path().join("dir/file"), b"later workload").unwrap();
    drop(engine);
    let mut engine = Transfers::open(dir.path(), ctx).unwrap();
    assert_eq!(
        engine.begin(request.clone()).unwrap().state,
        State::Committed
    );
    assert_eq!(
        engine.commit(request.operation_id).unwrap().state,
        State::Committed
    );
    assert_eq!(
        fs::read(dir.path().join("dir/file")).unwrap(),
        b"later workload"
    );
    request.path = "different".into();
    assert!(engine.begin(request).is_err());
}
#[test]
fn lost_chunk_reply_partial_write_and_restart_resume_without_replacing_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context();
    let mut engine = Transfers::open(dir.path(), ctx.clone()).unwrap();
    let request = upload("file", b"abcdefgh");
    let id = request.operation_id;
    engine.begin(request.clone()).unwrap();
    // Simulate a partial write before the sender received any acknowledgement.
    fs::OpenOptions::new()
        .append(true)
        .open(dir.path().join(STATE_DIRECTORY).join(format!("{id}.data")))
        .unwrap()
        .write_all(b"abc")
        .unwrap();
    assert_eq!(engine.write_chunk(id, 0, b"abcdef").unwrap(), 6);
    assert_eq!(engine.write_chunk(id, 0, b"abcdef").unwrap(), 6);
    assert!(engine.write_chunk(id, 0, b"changed").is_err());
    assert!(engine.write_chunk(id, 7, b"h").is_err());
    assert!(engine.write_chunk(id, u64::MAX, b"x").is_err());
    drop(engine);
    let mut engine = Transfers::open(dir.path(), ctx).unwrap();
    assert_eq!(engine.begin(request).unwrap().state, State::Staging);
    assert_eq!(engine.write_chunk(id, 6, b"gh").unwrap(), 8);
    engine.commit(id).unwrap();
    assert_eq!(fs::read(dir.path().join("file")).unwrap(), b"abcdefgh");
}
#[test]
fn bad_length_digest_and_chunks_never_change_the_destination() {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = Transfers::open(dir.path(), context()).unwrap();
    fs::write(dir.path().join("file"), b"old").unwrap();
    let request = upload("file", b"correct");
    let id = request.operation_id;
    engine.begin(request).unwrap();
    assert!(engine.write_chunk(id, 0, b"").is_err());
    assert!(
        engine
            .write_chunk(id, 0, &vec![0; MAX_CHUNK_BYTES + 1])
            .is_err()
    );
    assert!(engine.write_chunk(id, 0, b"too long").is_err());
    engine.write_chunk(id, 0, b"cor").unwrap();
    assert!(engine.commit(id).is_err());
    engine.write_chunk(id, 3, b"rupt").unwrap();
    assert!(engine.commit(id).is_err());
    assert_eq!(engine.inspect(id).unwrap().unwrap().state, State::Staging);
    assert_eq!(fs::read(dir.path().join("file")).unwrap(), b"old");
}
#[test]
fn crash_on_either_side_of_publication_stays_unknown_without_replay() {
    for published in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context();
        let mut engine = Transfers::open(dir.path(), ctx.clone()).unwrap();
        let request = upload("file", b"new");
        let id = request.operation_id;
        fs::write(dir.path().join("file"), b"old").unwrap();
        fill(&mut engine, &request, b"new");
        let mut receipt = engine.inspect(id).unwrap().unwrap();
        receipt.state = State::CommitIntent;
        drop(engine);
        // Reproduce the two durable crash windows: intent alone, or intent plus rename.
        let state = dir.path().join(STATE_DIRECTORY);
        fs::write(
            state.join(format!("{id}.json")),
            serde_json::to_vec(&receipt).unwrap(),
        )
        .unwrap();
        if published {
            fs::rename(state.join(format!("{id}.data")), dir.path().join("file")).unwrap();
        }
        let mut engine = Transfers::open(dir.path(), ctx.clone()).unwrap();
        assert_eq!(engine.inspect(id).unwrap().unwrap().state, State::Unknown);
        assert_eq!(engine.commit(id).unwrap().state, State::Unknown);
        assert_eq!(engine.begin(request).unwrap().state, State::Unknown);
        assert_eq!(
            fs::read(dir.path().join("file")).unwrap(),
            if published { b"new" } else { b"old" }
        );
        fs::write(dir.path().join("file"), b"later").unwrap();
        drop(engine);
        let mut engine = Transfers::open(dir.path(), ctx).unwrap();
        assert_eq!(engine.commit(id).unwrap().state, State::Unknown);
        assert_eq!(fs::read(dir.path().join("file")).unwrap(), b"later");
    }
}
#[test]
fn rename_failure_is_retained_unknown_and_never_retried() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context();
    let mut engine = Transfers::open(dir.path(), ctx.clone()).unwrap();
    let request = upload("occupied", b"new");
    let id = request.operation_id;
    fill(&mut engine, &request, b"new");
    fs::create_dir(dir.path().join("occupied")).unwrap();
    assert!(engine.commit(id).is_err());
    assert_eq!(engine.inspect(id).unwrap().unwrap().state, State::Unknown);
    fs::remove_dir(dir.path().join("occupied")).unwrap();
    assert_eq!(engine.commit(id).unwrap().state, State::Unknown);
    assert!(!dir.path().join("occupied").exists());
    drop(engine);
    assert_eq!(
        Transfers::open(dir.path(), ctx)
            .unwrap()
            .inspect(id)
            .unwrap()
            .unwrap()
            .state,
        State::Unknown
    );
}
#[test]
fn abort_reclaims_staging_but_retains_identity_and_budget() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context();
    let mut engine = Transfers::open(dir.path(), ctx.clone()).unwrap();
    let request = upload("file", b"data");
    let id = request.operation_id;
    fill(&mut engine, &request, b"data");
    assert_eq!(engine.abort(id).unwrap().state, State::Aborted);
    assert!(
        !dir.path()
            .join(STATE_DIRECTORY)
            .join(format!("{id}.data"))
            .exists()
    );
    assert!(engine.write_chunk(id, 0, b"data").is_err());
    assert_eq!(engine.begin(request.clone()).unwrap().state, State::Aborted);
    assert_eq!(engine.commit(id).unwrap().state, State::Aborted);
    drop(engine);
    assert_eq!(
        Transfers::open(dir.path(), ctx)
            .unwrap()
            .begin(request)
            .unwrap()
            .state,
        State::Aborted
    );
}
#[test]
fn empty_files_and_retained_limits_apply_before_admission() {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = Transfers::open(dir.path(), context()).unwrap();
    for index in 0..MAX_TRANSFERS {
        let request = upload(&format!("file-{index}"), b"");
        engine.begin(request.clone()).unwrap();
        engine.commit(request.operation_id).unwrap();
    }
    assert!(engine.begin(upload("overflow", b"")).is_err());
    let dir2 = tempfile::tempdir().unwrap();
    let mut engine2 = Transfers::open(dir2.path(), context()).unwrap();
    for index in 0..8 {
        let mut request = upload(&format!("file-{index}"), b"");
        request.size = MAX_FILE_BYTES;
        engine2.begin(request.clone()).unwrap();
        engine2.abort(request.operation_id).unwrap();
    }
    assert!(engine2.begin(upload("overflow", b"x")).is_err());
}
#[test]
fn context_single_owner_and_corrupt_or_missing_history_fail_closed() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context();
    let mut engine = Transfers::open(dir.path(), ctx.clone()).unwrap();
    assert!(Transfers::open(dir.path(), ctx.clone()).is_err());
    let request = upload("file", b"data");
    let id = request.operation_id;
    engine.begin(request).unwrap();
    drop(engine);
    assert!(Transfers::open(dir.path(), context()).is_err());
    fs::remove_file(dir.path().join(STATE_DIRECTORY).join(format!("{id}.data"))).unwrap();
    assert!(Transfers::open(dir.path(), ctx.clone()).is_err());
    fs::write(
        dir.path().join(STATE_DIRECTORY).join(format!("{id}.data")),
        b"",
    )
    .unwrap();
    fs::write(
        dir.path().join(STATE_DIRECTORY).join(format!("{id}.json")),
        b"invalid",
    )
    .unwrap();
    assert!(Transfers::open(dir.path(), ctx).is_err());
}
#[test]
fn traversal_symlink_parents_and_reserved_paths_never_touch_outside_files() {
    let dir = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("victim"), b"outside").unwrap();
    symlink(outside.path(), dir.path().join("link")).unwrap();
    let mut engine = Transfers::open(dir.path(), context()).unwrap();
    for path in [
        "../victim",
        "/victim",
        "link/victim",
        ".hudson-transfers/context.json",
        "a/../victim",
    ] {
        assert!(engine.begin(upload(path, b"changed")).is_err());
        assert!(engine.capture(path).is_err());
    }
    assert_eq!(fs::read(outside.path().join("victim")).unwrap(), b"outside");
}
#[test]
fn parent_symlink_substitution_between_stage_and_commit_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join("dir")).unwrap();
    let mut engine = Transfers::open(dir.path(), context()).unwrap();
    let request = upload("dir/victim", b"data");
    fill(&mut engine, &request, b"data");
    fs::rename(dir.path().join("dir"), dir.path().join("old-dir")).unwrap();
    symlink(outside.path(), dir.path().join("dir")).unwrap();
    assert!(engine.commit(request.operation_id).is_err());
    assert_eq!(
        engine.inspect(request.operation_id).unwrap().unwrap().state,
        State::Staging
    );
    assert!(!outside.path().join("victim").exists());
}
#[test]
fn replacement_does_not_follow_destination_symlinks_or_modify_hardlink_targets() {
    let dir = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("victim"), b"outside").unwrap();
    symlink(outside.path().join("victim"), dir.path().join("link")).unwrap();
    fs::hard_link(outside.path().join("victim"), dir.path().join("hard")).unwrap();
    let mut engine = Transfers::open(dir.path(), context()).unwrap();
    assert!(engine.capture("link").is_err());
    assert!(engine.capture("hard").is_err());
    for path in ["link", "hard"] {
        let request = upload(path, b"new");
        fill(&mut engine, &request, b"new");
        engine.commit(request.operation_id).unwrap();
        assert_eq!(fs::read(dir.path().join(path)).unwrap(), b"new");
    }
    assert_eq!(fs::read(outside.path().join("victim")).unwrap(), b"outside");
}
#[test]
fn fifo_socket_directory_and_magic_link_downloads_are_rejected_without_opening() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Transfers::open(dir.path(), context()).unwrap();
    let root = fs::File::open(dir.path()).unwrap();
    rustix::fs::mknodat(
        &root,
        "fifo",
        rustix::fs::FileType::Fifo,
        rustix::fs::Mode::from_bits_retain(0o600),
        0,
    )
    .unwrap();
    let _socket = std::os::unix::net::UnixListener::bind(dir.path().join("socket")).unwrap();
    fs::create_dir(dir.path().join("directory")).unwrap();
    symlink("/proc/self/fd/0", dir.path().join("magic")).unwrap();
    let start = std::time::Instant::now();
    for path in ["fifo", "socket", "directory", "magic"] {
        assert!(engine.capture(path).is_err());
    }
    assert!(start.elapsed() < std::time::Duration::from_secs(2));
}
#[test]
fn captured_downloads_are_bounded_stable_and_keep_workspace_ownership() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context();
    let engine = Transfers::open(dir.path(), ctx.clone()).unwrap();
    fs::write(dir.path().join("file"), b"original").unwrap();
    let mut captures: Vec<_> = (0..8).map(|_| engine.capture("file").unwrap()).collect();
    assert!(engine.capture("file").is_err());
    fs::write(dir.path().join("file"), b"changed").unwrap();
    assert_eq!(captures[0].chunk(0, MAX_CHUNK_BYTES).unwrap(), b"original");
    assert_eq!(
        captures[0].sha256,
        <[u8; 32]>::from(Sha256::digest(b"original"))
    );
    assert!(captures[0].chunk(u64::MAX, 1).is_err());
    assert!(captures[0].chunk(0, MAX_CHUNK_BYTES + 1).is_err());
    assert_eq!(captures[0].chunk(8, 1).unwrap(), b"");
    captures.pop();
    assert_eq!(
        engine.capture("file").unwrap().chunk(0, 7).unwrap(),
        b"changed"
    );
    drop(engine);
    assert!(Transfers::open(dir.path(), ctx.clone()).is_err());
    drop(captures);
    Transfers::open(dir.path(), ctx).unwrap();
}
#[test]
fn oversized_download_and_substituted_stage_never_open_the_external_target() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context();
    let mut engine = Transfers::open(dir.path(), ctx.clone()).unwrap();
    fs::File::create(dir.path().join("huge"))
        .unwrap()
        .set_len(MAX_FILE_BYTES + 1)
        .unwrap();
    assert!(engine.capture("huge").is_err());
    let outside = tempfile::NamedTempFile::new().unwrap();
    fs::write(outside.path(), b"safe").unwrap();
    let request = upload("file", b"data");
    let id = request.operation_id;
    engine.begin(request).unwrap();
    let staged = dir.path().join(STATE_DIRECTORY).join(format!("{id}.data"));
    fs::remove_file(&staged).unwrap();
    symlink(outside.path(), &staged).unwrap();
    assert!(engine.write_chunk(id, 0, b"data").is_err());
    assert!(engine.commit(id).is_err());
    assert_eq!(fs::read(outside.path()).unwrap(), b"safe");
    drop(engine);
    assert!(Transfers::open(dir.path(), ctx).is_err());
}

#[test]
fn maximum_file_round_trip_and_download_debug_do_not_expose_content() {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = Transfers::open(dir.path(), context()).unwrap();
    let bytes = vec![97; MAX_FILE_BYTES as usize];
    let request = upload("large", &bytes);
    fill(&mut engine, &request, &bytes);
    engine.commit(request.operation_id).unwrap();
    let captured = engine.capture("large").unwrap();
    let mut hash = Sha256::new();
    let mut offset = 0;
    while offset < captured.size() {
        let chunk = captured.chunk(offset, MAX_CHUNK_BYTES).unwrap();
        hash.update(chunk);
        offset += chunk.len() as u64;
    }
    assert_eq!(captured.size(), MAX_FILE_BYTES);
    assert_eq!(<[u8; 32]>::from(hash.finalize()), request.sha256);
    assert!(!format!("{captured:?}").contains("aaaa"));
}

#[test]
fn metadata_failure_fences_engine_before_destination_mutation() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context();
    let mut engine = Transfers::open(dir.path(), ctx.clone()).unwrap();
    fs::write(dir.path().join("file"), b"old").unwrap();
    let request = upload("file", b"new");
    let id = request.operation_id;
    fill(&mut engine, &request, b"new");
    let receipt_path = dir.path().join(STATE_DIRECTORY).join(format!("{id}.json"));
    let saved = fs::read(&receipt_path).unwrap();
    fs::remove_file(&receipt_path).unwrap();
    fs::create_dir(&receipt_path).unwrap();
    assert!(engine.commit(id).is_err());
    assert!(engine.inspect(id).is_err());
    assert!(engine.begin(upload("another", b"")).is_err());
    assert_eq!(fs::read(dir.path().join("file")).unwrap(), b"old");
    drop(engine);
    fs::remove_dir(&receipt_path).unwrap();
    fs::write(&receipt_path, saved).unwrap();
    let mut recovered = Transfers::open(dir.path(), ctx).unwrap();
    assert_eq!(recovered.commit(id).unwrap().state, State::Committed);
    assert_eq!(fs::read(dir.path().join("file")).unwrap(), b"new");
}
