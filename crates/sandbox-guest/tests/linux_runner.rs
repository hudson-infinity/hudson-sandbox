//! Explicitly opt-in: these tests run controlled commands as root in a dedicated Linux VM.
#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used, clippy::expect_used)]
use sandbox_guest::{
    model::{Context, Execute, MAX_OUTPUT, Receipt, State},
    runner::{Config, Runner, now_ms},
};
use sandbox_protocol::{AllocationId, Id, OperationId};
use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    path::PathBuf,
    process::{Command, Stdio},
    time::Duration,
};

struct Fixture {
    state: tempfile::TempDir,
    group: PathBuf,
    context: Context,
}
impl Fixture {
    fn new() -> Self {
        assert_eq!(
            std::env::var("HUDSON_GUEST_TEST_VM").as_deref(),
            Ok("1"),
            "requires explicit dedicated-VM opt-in"
        );
        assert!(rustix::process::geteuid().is_root());
        let group = PathBuf::from(format!(
            "/sys/fs/cgroup/hudson-guest-tests-{}",
            OperationId::generate()
        ));
        fs::write("/sys/fs/cgroup/cgroup.subtree_control", "+pids").unwrap();
        fs::create_dir(&group).unwrap();
        fs::write(group.join("cgroup.subtree_control"), "+pids").unwrap();
        Self {
            state: tempfile::tempdir().unwrap(),
            group,
            context: Context {
                allocation_id: AllocationId::generate(),
                generation: 1,
                boot_id: fs::read_to_string("/proc/sys/kernel/random/boot_id")
                    .unwrap()
                    .trim()
                    .into(),
            },
        }
    }
    fn config(&self) -> Config {
        Config {
            state_dir: self.state.path().into(),
            cgroup_root: self.group.clone(),
            launcher: env!("CARGO_BIN_EXE_sandbox-guest").into(),
            context: self.context.clone(),
        }
    }
    fn request(&self, script: &str) -> Execute {
        Execute {
            operation_id: OperationId::generate(),
            argv: vec!["/bin/sh".into(), "-c".into(), script.into()],
            env: BTreeMap::new(),
            cwd: self.state.path().to_str().unwrap().into(),
            deadline_unix_ms: now_ms() + 10000,
            output_limit: 4096,
        }
    }
    fn output(&self, id: OperationId, stream: &str) -> Vec<u8> {
        fs::read(self.state.path().join(format!("{id}.{stream}"))).unwrap()
    }
    fn assert_empty(&self) {
        assert!(
            !fs::read_dir(&self.group)
                .unwrap()
                .any(|v| v.unwrap().file_type().unwrap().is_dir())
        );
    }
    fn cli(&self, request: &Execute) -> std::process::Child {
        let mut child = Command::new(env!("CARGO_BIN_EXE_sandbox-guest"))
            .args([
                "--state-dir",
                self.state.path().to_str().unwrap(),
                "--cgroup-root",
                self.group.to_str().unwrap(),
                "--allocation-id",
                &self.context.allocation_id.to_string(),
                "--generation",
                "1",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(&serde_json::to_vec(request).unwrap())
            .unwrap();
        child
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::write(self.group.join("cgroup.kill"), "1");
        fn remove(path: &std::path::Path) {
            if let Ok(entries) = fs::read_dir(path) {
                for entry in entries.flatten() {
                    if entry.file_type().is_ok_and(|t| t.is_dir()) {
                        remove(&entry.path());
                    }
                }
            }
            let _ = fs::remove_dir(path);
        }
        remove(&self.group);
    }
}
async fn finished(runner: &Runner, id: OperationId) -> Receipt {
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let r = runner.inspect(id).await.unwrap();
            if r.state.terminal() {
                assert!(r.cleanup_confirmed, "{r:?}");
                return r;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("command completed with cleanup")
}
async fn marker(path: &std::path::Path) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while !path.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("marker missing: {path:?}"));
}

#[tokio::test]
#[ignore = "requires root and HUDSON_GUEST_TEST_VM=1 in a dedicated Linux development VM"]
async fn exit_bytes_environment_and_guest_root() {
    let f = Fixture::new();
    let runner = Runner::open(f.config()).await.unwrap();
    let r=f.request("test \"$(id -u)\" = 0 && test -z \"$HUDSON_GUEST_TEST_VM\" && test \"$CUSTOM\" = allowed && touch writable-root && printf '\\000\\377' && printf error >&2; exit 23");
    let mut r = r;
    r.env.insert("CUSTOM".into(), "allowed".into());
    runner.start(r.clone()).await.unwrap();
    let receipt = finished(&runner, r.operation_id).await;
    assert_eq!(receipt.state, State::Exited);
    assert_eq!(receipt.exit_code, Some(23));
    assert_eq!(f.output(r.operation_id, "stdout"), [0, 255]);
    assert_eq!(f.output(r.operation_id, "stderr"), b"error");
    assert!(f.state.path().join("writable-root").exists());
    f.assert_empty();
    let mut r = f.request("");
    r.argv = vec!["/bin/echo".into(), "$(touch must-not-exist)".into()];
    runner.start(r.clone()).await.unwrap();
    assert_eq!(finished(&runner, r.operation_id).await.exit_code, Some(0));
    assert!(!f.state.path().join("must-not-exist").exists());
    let r = f.request("kill -KILL $$");
    runner.start(r.clone()).await.unwrap();
    assert_eq!(finished(&runner, r.operation_id).await.signal, Some(9));
    let r = f.request("ls /proc/[0-9]*/comm | wc -l");
    runner.start(r.clone()).await.unwrap();
    assert_eq!(finished(&runner, r.operation_id).await.exit_code, Some(0));
    let count = String::from_utf8(f.output(r.operation_id, "stdout"))
        .unwrap()
        .trim()
        .parse::<u32>()
        .unwrap();
    assert!(
        count < 12,
        "outer VM processes leaked into namespace: {count}"
    );
}
#[tokio::test]
#[ignore = "requires root and HUDSON_GUEST_TEST_VM=1 in a dedicated Linux development VM"]
async fn output_budget_drains_both_streams_and_process_tree_is_reaped() {
    let f = Fixture::new();
    let runner = Runner::open(f.config()).await.unwrap();
    let mut r=f.request("head -c 200000 /dev/zero; head -c 200000 /dev/zero >&2; setsid sh -c 'sleep 30' >/dev/null 2>&1 & touch completed");
    r.output_limit = 1024;
    runner.start(r.clone()).await.unwrap();
    let receipt = finished(&runner, r.operation_id).await;
    assert_eq!(receipt.state, State::Exited);
    assert_eq!(receipt.exit_code, Some(0));
    assert_eq!(receipt.stdout.seen + receipt.stderr.seen, 400000);
    assert_eq!(receipt.stdout.stored + receipt.stderr.stored, 1024);
    assert!(receipt.stdout.truncated && receipt.stderr.truncated);
    assert!(f.state.path().join("completed").exists());
    f.assert_empty();
}
#[tokio::test]
#[ignore = "requires root and HUDSON_GUEST_TEST_VM=1 in a dedicated Linux development VM"]
async fn deadlines_cancel_conflicts_and_retry_never_replay() {
    let f = Fixture::new();
    let runner = Runner::open(f.config()).await.unwrap();
    let mut r = f.request("echo once >> launches; touch started; setsid sh -c 'sleep 30' & wait");
    r.deadline_unix_ms = now_ms() + 800;
    let (a, b) = tokio::join!(runner.start(r.clone()), runner.start(r.clone()));
    assert_eq!(a.unwrap().operation_id, b.unwrap().operation_id);
    let mut changed = r.clone();
    changed.argv.push("changed".into());
    assert!(runner.start(changed).await.is_err());
    assert!(runner.start(f.request("true")).await.is_err());
    let result = finished(&runner, r.operation_id).await;
    assert_eq!(result.state, State::TimedOut);
    f.assert_empty();
    assert_eq!(
        runner.start(r.clone()).await.unwrap().state,
        State::TimedOut
    );
    assert_eq!(
        fs::read_to_string(f.state.path().join("launches")).unwrap(),
        "once\n"
    );
    let mut expired = f.request("touch rejected");
    expired.deadline_unix_ms = now_ms() - 1;
    assert!(runner.start(expired).await.is_err());
    assert!(!f.state.path().join("rejected").exists());
    let r = f.request("touch cancel-started; sleep 30");
    runner.start(r.clone()).await.unwrap();
    marker(&f.state.path().join("cancel-started")).await;
    runner.cancel(r.operation_id).await.unwrap();
    let result = finished(&runner, r.operation_id).await;
    assert_eq!(result.state, State::Cancelled);
    assert!(result.cancel_requested);
    f.assert_empty();
}
#[tokio::test]
#[ignore = "requires root and HUDSON_GUEST_TEST_VM=1 in a dedicated Linux development VM"]
async fn restart_kills_owned_tree_and_returns_unknown_without_replay() {
    let f = Fixture::new();
    let r = f.request("echo once >> launches; touch started; sleep 30");
    let mut child = f.cli(&r);
    marker(&f.state.path().join("started")).await;
    assert!(Runner::open(f.config()).await.is_err());
    child.kill().unwrap();
    child.wait().unwrap();
    let runner = Runner::open(f.config()).await.unwrap();
    let result = runner.start(r.clone()).await.unwrap();
    assert_eq!(result.state, State::Unknown);
    assert!(result.cleanup_confirmed);
    f.assert_empty();
    assert_eq!(
        fs::read_to_string(f.state.path().join("launches")).unwrap(),
        "once\n"
    );
    drop(runner);
    let mut config = f.config();
    config.context.generation += 1;
    assert!(Runner::open(config).await.is_err());
    let mut config = f.config();
    config.context.boot_id = "different".into();
    assert!(Runner::open(config).await.is_err());
}
#[tokio::test]
#[ignore = "requires root and HUDSON_GUEST_TEST_VM=1 in a dedicated Linux development VM"]
async fn retention_is_bounded_and_corrupt_receipts_fail_closed() {
    let f = Fixture::new();
    let runner = Runner::open(f.config()).await.unwrap();
    let mut ids = Vec::new();
    for _ in 0..4 {
        let mut r = f.request("true");
        r.output_limit = MAX_OUTPUT;
        runner.start(r.clone()).await.unwrap();
        assert_eq!(finished(&runner, r.operation_id).await.exit_code, Some(0));
        ids.push(r.operation_id);
    }
    assert!(runner.start(f.request("true")).await.is_err());
    drop(runner);
    let path = f.state.path().join(format!("{}.json", ids[0]));
    let mut value: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    value["stdout"]["stored"] = (MAX_OUTPUT + 1).into();
    fs::write(path, serde_json::to_vec(&value).unwrap()).unwrap();
    assert!(Runner::open(f.config()).await.is_err());
}

#[tokio::test]
#[ignore = "requires root and HUDSON_GUEST_TEST_VM=1 in a dedicated Linux development VM"]
async fn failed_receipt_commit_is_unknown_and_disables_new_work() {
    let f = Fixture::new();
    let runner = Runner::open(f.config()).await.unwrap();
    let r = f.request("touch started; sleep .3; exit 0");
    runner.start(r.clone()).await.unwrap();
    marker(&f.state.path().join("started")).await;
    // Hide the state directory to inject a terminal journal failure after the process started.
    let moved = f.state.path().with_extension("moved");
    fs::rename(f.state.path(), &moved).unwrap();
    let receipt = finished(&runner, r.operation_id).await;
    assert_eq!(receipt.state, State::Unknown);
    assert!(runner.start(f.request("true")).await.is_err());
    fs::rename(&moved, f.state.path()).unwrap();
    drop(runner);
    let reopened = Runner::open(f.config()).await.unwrap();
    assert_eq!(reopened.start(r).await.unwrap().state, State::Unknown);
    f.assert_empty();
}

#[tokio::test]
#[ignore = "requires root and HUDSON_GUEST_TEST_VM=1 in a dedicated Linux development VM"]
async fn dropping_caller_keeps_work_owned_and_failed_spawn_is_not_success() {
    let f = Fixture::new();
    let runner = Runner::open(f.config()).await.unwrap();
    let r = f.request("sleep .1; touch survived");
    let observer = runner.clone();
    runner.start(r.clone()).await.unwrap();
    drop(runner);
    assert_eq!(finished(&observer, r.operation_id).await.exit_code, Some(0));
    assert!(f.state.path().join("survived").exists());
    let mut r = f.request("");
    r.argv = vec!["/no-such-executable".into()];
    observer.start(r.clone()).await.unwrap();
    let receipt = finished(&observer, r.operation_id).await;
    assert_eq!(receipt.state, State::Unknown);
    assert_eq!(receipt.exit_code, None);
    f.assert_empty();
}

#[path = "../../sandbox-protocol/tests/support/guest_tls.rs"]
mod wire_tls;

#[tokio::test]
#[ignore = "requires root and HUDSON_GUEST_TEST_VM=1 in a dedicated Linux development VM"]
async fn authenticated_host_calls_execute_inspect_cancel_output_and_survive_disconnect() {
    use sandbox_protocol::{
        guest as w,
        guest_wire::{self as wire, ClientTls},
    };
    use sandbox_supervisor::guest::GuestClient;
    use tokio::{io::AsyncReadExt, net::UnixListener};
    let mut f = Fixture::new();
    let certs = wire_tls::Fixture::new();
    f.context.allocation_id = certs.allocation;
    let runner = Runner::open(f.config()).await.unwrap();
    let socket_dir = tempfile::tempdir().unwrap();
    let socket = socket_dir.path().join("vsock.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let server_runner = runner.clone();
    let tls = certs.server();
    let serving = tokio::spawn(async move {
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            let runner = server_runner.clone();
            let tls = tls.clone();
            tokio::spawn(async move {
                let mut header = [0; 11];
                stream.read_exact(&mut header).await.unwrap();
                assert_eq!(&header, b"CONNECT 52\n");
                use tokio::io::AsyncWriteExt;
                stream.write_all(b"OK 12345\n").await.unwrap();
                let _ = sandbox_guest::server::serve_connection(&runner, &tls, stream).await;
            });
        }
    });
    let make = |context: Context, tls: ClientTls| {
        GuestClient::new(socket.clone(), 52, tls, context).unwrap()
    };
    let mut discovery = f.context.clone();
    discovery.boot_id.clear();
    let unbound = make(discovery, certs.client());
    assert!(unbound.execute(&f.request("true")).await.is_err());
    let bound = unbound.hello().await.unwrap();
    assert_eq!(bound, f.context);
    let client = make(bound, certs.client());
    let r = f.request("printf wire-output; touch wire-started; sleep 30");
    client.execute(&r).await.unwrap();
    marker(&f.state.path().join("wire-started")).await;
    assert_eq!(
        client.execute(&r).await.unwrap().operation_id,
        r.operation_id
    );
    let mut changed = r.clone();
    changed.argv.push("conflict".into());
    assert!(client.execute(&changed).await.is_err());
    assert_eq!(
        client.inspect(r.operation_id).await.unwrap().state,
        State::LaunchIntent
    );
    client.cancel(r.operation_id).await.unwrap();
    finished(&runner, r.operation_id).await;
    assert_eq!(
        client.inspect(r.operation_id).await.unwrap().state,
        State::Cancelled
    );
    let output = client
        .output(w::ReadOutput {
            operation_id: r.operation_id.to_string(),
            stream: w::Stream::Stdout as i32,
            offset: 0,
            limit: 4,
        })
        .await
        .unwrap();
    assert_eq!(output.data, b"wire");
    assert!(!output.at_end);
    assert!(output.complete);
    let output = client
        .output(w::ReadOutput {
            operation_id: r.operation_id.to_string(),
            stream: w::Stream::Stdout as i32,
            offset: 4,
            limit: 32,
        })
        .await
        .unwrap();
    assert_eq!(output.data, b"-output");
    assert!(output.at_end);
    let mut stale = f.context.clone();
    stale.boot_id = "stale".into();
    assert!(
        make(stale, certs.client())
            .inspect(r.operation_id)
            .await
            .is_err()
    );
    let mut wrong = f.context.clone();
    wrong.allocation_id = AllocationId::generate();
    assert!(
        make(wrong, certs.client())
            .inspect(r.operation_id)
            .await
            .is_err()
    );
    // Send a complete execute then close without reading its acknowledgement.
    let lost = f.request("echo once >> wire-lost; touch wire-lost-started; sleep .1");
    let (a, b) = tokio::io::duplex(32768);
    let server_runner = runner.clone();
    let tls = certs.server();
    let task = tokio::spawn(async move {
        let _ = sandbox_guest::server::serve_connection(&server_runner, &tls, a).await;
    });
    let mut stream = certs.client().connect(b).await.unwrap();
    wire::write_frame(
        &mut stream,
        &w::Request {
            version: 1,
            request_id: OperationId::generate().to_string(),
            context: Some((&f.context).into()),
            action: Some(w::request::Action::Execute((&lost).into())),
        },
    )
    .await
    .unwrap();
    // Establish admission from the guest-side marker, while deliberately never reading the reply.
    marker(&f.state.path().join("wire-lost-started")).await;
    drop(stream);
    finished(&runner, lost.operation_id).await;
    task.await.unwrap();
    assert_eq!(client.execute(&lost).await.unwrap().exit_code, Some(0));
    assert_eq!(
        fs::read_to_string(f.state.path().join("wire-lost")).unwrap(),
        "once\n"
    );
    serving.abort();
    runner.shutdown().await.unwrap();
    f.assert_empty();
}

#[tokio::test]
#[ignore = "requires root and HUDSON_GUEST_TEST_VM=1 in a dedicated Linux development VM"]
async fn output_rejects_symlinks_nonregular_files_and_offsets_outside_retained_bytes() {
    use sandbox_protocol::guest as w;
    let f = Fixture::new();
    let runner = Runner::open(f.config()).await.unwrap();
    let r = f.request("printf retained");
    runner.start(r.clone()).await.unwrap();
    finished(&runner, r.operation_id).await;
    let request = w::ReadOutput {
        operation_id: r.operation_id.to_string(),
        stream: w::Stream::Stdout as i32,
        offset: 100,
        limit: 32,
    };
    assert!(runner.output(&request).await.is_err());
    let path = f.state.path().join(format!("{}.stdout", r.operation_id));
    fs::remove_file(&path).unwrap();
    std::os::unix::fs::symlink("/etc/passwd", &path).unwrap();
    let mut request = request;
    request.offset = 0;
    assert!(runner.output(&request).await.is_err());
    fs::remove_file(&path).unwrap();
    fs::create_dir(&path).unwrap();
    assert!(runner.output(&request).await.is_err());
    fs::remove_dir(&path).unwrap();
    request.operation_id = "../../etc/passwd".into();
    assert!(runner.output(&request).await.is_err());
    let active = f.request("sleep 30");
    runner.start(active.clone()).await.unwrap();
    runner.shutdown().await.unwrap();
    assert_eq!(
        runner.inspect(active.operation_id).await.unwrap().state,
        State::Cancelled
    );
    let empty = runner
        .output(&w::ReadOutput {
            operation_id: active.operation_id.to_string(),
            stream: w::Stream::Stdout as i32,
            offset: 0,
            limit: 32,
        })
        .await
        .unwrap();
    assert!(empty.data.is_empty() && empty.complete && empty.at_end);
    assert!(
        runner
            .start(f.request("touch after-shutdown"))
            .await
            .is_err()
    );
    assert!(!f.state.path().join("after-shutdown").exists());
    f.assert_empty();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires root and HUDSON_GUEST_TEST_VM=1 in a dedicated Linux development VM"]
async fn shutdown_fences_concurrent_admission_before_releasing_ownership() {
    let f = Fixture::new();
    let runner = Runner::open(f.config()).await.unwrap();
    let request = f.request("sleep 30");
    let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(3));
    let admitting = runner.clone();
    let gate = barrier.clone();
    let input = request.clone();
    let start = tokio::spawn(async move {
        gate.wait().await;
        admitting.start(input).await
    });
    let draining = runner.clone();
    let gate = barrier.clone();
    let stop = tokio::spawn(async move {
        gate.wait().await;
        draining.shutdown().await
    });
    barrier.wait().await;
    let admitted = start.await.unwrap();
    let stopped = stop.await.unwrap();
    stopped.unwrap();
    if admitted.is_ok() {
        assert_eq!(
            runner.inspect(request.operation_id).await.unwrap().state,
            State::Cancelled
        );
    }
    assert!(runner.start(f.request("true")).await.is_err());
    f.assert_empty();
}

#[tokio::test]
#[ignore = "requires root and HUDSON_GUEST_TEST_VM=1 in a dedicated Linux VM"]
async fn live_capture_counters_bound_reads_before_terminal_receipt() {
    use sandbox_protocol::guest::{ReadOutput, Stream};
    let f = Fixture::new();
    let runner = Runner::open(f.config()).await.unwrap();
    let request = f.request("printf '\\000\\377a'; printf '\\376e' >&2; sleep 2; printf z");
    let id = request.operation_id;
    runner.start(request).await.unwrap();
    let until = tokio::time::Instant::now() + Duration::from_secs(1);
    loop {
        let r = runner.inspect(id).await.unwrap();
        r.validate().unwrap();
        if r.stdout.stored == 3 && r.stderr.stored == 2 {
            assert_eq!(r.state, State::LaunchIntent);
            break;
        }
        assert!(tokio::time::Instant::now() < until);
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let mut read = ReadOutput {
        operation_id: id.to_string(),
        stream: Stream::Stdout as i32,
        offset: 0,
        limit: 32,
    };
    let chunk = runner.output(&read).await.unwrap();
    assert_eq!(chunk.data, [0, 255, b'a']);
    assert!(chunk.at_end && !chunk.complete);
    read.offset = 3;
    let end = runner.output(&read).await.unwrap();
    assert!(end.data.is_empty() && !end.complete);
    // Bytes outside the captured prefix are not exposed, even if present in the file.
    let path = f.state.path().join(format!("{id}.stdout"));
    fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(b"unaccounted")
        .unwrap();
    let end = runner.output(&read).await.unwrap();
    assert!(end.data.is_empty() && !end.complete);
    fs::OpenOptions::new()
        .write(true)
        .open(&path)
        .unwrap()
        .set_len(3)
        .unwrap();
    let until = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let r = runner.inspect(id).await.unwrap();
        if r.cleanup_confirmed {
            assert_eq!(r.state, State::Exited);
            assert_eq!(r.stdout.stored, 4);
            assert_eq!(r.stderr.stored, 2);
            break;
        }
        assert!(tokio::time::Instant::now() < until);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let final_chunk = runner.output(&read).await.unwrap();
    assert_eq!(final_chunk.data, b"z");
    assert!(final_chunk.complete);
    f.assert_empty();
}
