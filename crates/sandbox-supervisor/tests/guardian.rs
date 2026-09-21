//! Controlled root-only Firecracker tests on the dedicated development host.
#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used, clippy::expect_used)]
use sandbox_protocol::{AllocationId, HostId, Id, OperationId, ProjectId, SandboxId};
use sandbox_supervisor::guardian::{
    self, Action, Artifact, Config, Manifest, Owner, Receipt, Start, State,
};
use sha2::{Digest, Sha256};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

struct Fixture {
    temp: tempfile::TempDir,
    manifest: Manifest,
    path: PathBuf,
}
fn artifact(path: PathBuf) -> Artifact {
    Artifact {
        sha256: hex::encode(Sha256::digest(fs::read(&path).unwrap())),
        path,
    }
}
impl Fixture {
    fn new(ttl_ms: i64) -> Self {
        Self::build(ttl_ms, false)
    }
    fn build(ttl_ms: i64, agent: bool) -> Self {
        assert_eq!(std::env::var("HUDSON_GUARDIAN_TEST_VM").as_deref(), Ok("1"));
        assert!(rustix::process::geteuid().is_root());
        let temp = tempfile::Builder::new().prefix("hg-").tempdir().unwrap();
        let root = temp.path();
        let image = root.join("image");
        fs::create_dir(&image).unwrap();
        fs::create_dir(image.join("bin")).unwrap();
        fs::copy("/bin/busybox", image.join("bin/busybox")).unwrap();
        fs::set_permissions(image.join("bin/busybox"), fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(
            image.join("init"),
            "#!/bin/busybox sh\nwhile :; do :; done\n",
        )
        .unwrap();
        if agent {
            // Only the locally built, reviewed binary is passed to ldd. Never run
            // ldd against customer-selected binaries or mount their filesystems.
            let binary = Path::new("/home/safal.guest/hudson-sandbox/target/release/sandbox-guest");
            fs::copy(binary, image.join("init")).unwrap();
            let libraries = Command::new("/usr/bin/ldd").arg(binary).output().unwrap();
            assert!(libraries.status.success());
            for word in std::str::from_utf8(&libraries.stdout)
                .unwrap()
                .split_whitespace()
            {
                if word.starts_with('/') {
                    let dest = image.join(word.trim_start_matches('/'));
                    fs::create_dir_all(dest.parent().unwrap()).unwrap();
                    fs::copy(word, dest).unwrap();
                }
            }
        }
        fs::set_permissions(image.join("init"), fs::Permissions::from_mode(0o755)).unwrap();
        let rootfs = root.join("source.ext4");
        fs::File::create(&rootfs)
            .unwrap()
            .set_len(64 << 20)
            .unwrap();
        assert!(
            Command::new("/usr/sbin/mkfs.ext4")
                .args(["-q", "-F", "-d"])
                .arg(image)
                .arg(&rootfs)
                .status()
                .unwrap()
                .success()
        );
        let artifacts = Path::new("/tmp/hudson-fc-artifacts");
        let manifest = Manifest {
            config: Config {
                state_root: root.join("s"),
                cgroup_parent: PathBuf::from("/sys/fs/cgroup/hudson-guardians-tests"),
                firecracker: artifact(
                    artifacts.join("release-v1.17.0-aarch64/firecracker-v1.17.0-aarch64"),
                ),
                jailer: artifact(artifacts.join("release-v1.17.0-aarch64/jailer-v1.17.0-aarch64")),
                kernel: artifact(artifacts.join("vmlinux-6.1.186")),
                rootfs: artifact(rootfs),
                jail_uid: 65534,
                jail_gid: 65534,
            },
            start: Start {
                owner: Owner {
                    host: HostId::generate(),
                    project: ProjectId::generate(),
                    sandbox: SandboxId::generate(),
                    allocation: AllocationId::generate(),
                    create_operation: OperationId::generate(),
                    generation: 1,
                    epoch: 1,
                },
                vcpu: 1,
                memory_mib: 128,
                disk_mib: 64,
                expires_unix_ms: guardian::wall_ms() + ttl_ms,
            },
        };
        let path = root.join("start.json");
        fs::write(&path, serde_json::to_vec(&manifest).unwrap()).unwrap();
        Self {
            temp,
            manifest,
            path,
        }
    }
    fn spawn(&self) -> Child {
        Command::new(env!("CARGO_BIN_EXE_sandbox-supervisor"))
            .arg("--manifest")
            .arg(&self.path)
            .arg("run")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    }
    fn record(&self) -> Receipt {
        guardian::read_json(&self.manifest.directory().join("receipt.json")).unwrap()
    }
    fn running(&self, child: &mut Child) -> Receipt {
        let deadline = Instant::now() + Duration::from_secs(8);
        loop {
            if let Ok(reply) = guardian::control(&self.manifest, Action::Inspect)
                && let Some(r) = reply.receipt
                && r.state == State::Running
            {
                return r;
            }
            if let Some(status) = child.try_wait().unwrap() {
                let output = child
                    .stderr
                    .take()
                    .map(|mut v| {
                        use std::io::Read;
                        let mut s = String::new();
                        v.read_to_string(&mut s).unwrap();
                        s
                    })
                    .unwrap_or_default();
                panic!(
                    "guardian exited early {status}: {output}; receipt={:?}",
                    self.record()
                );
            }
            assert!(
                Instant::now() < deadline,
                "guardian never became running; {:?}",
                self.record()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    fn ready(&self) -> Receipt {
        let until = Instant::now() + Duration::from_secs(15);
        loop {
            if let Ok(response) = guardian::control(&self.manifest, Action::BindGuest)
                && let Some(receipt) = response.receipt
                && receipt.guest_boot_id.is_some()
            {
                return receipt;
            }
            assert!(
                Instant::now() < until,
                "guest boot never bound: {:?}",
                self.record()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    fn stopped(&self) -> Receipt {
        let deadline = Instant::now() + Duration::from_secs(8);
        loop {
            if let Ok(r) =
                guardian::read_json::<Receipt>(&self.manifest.directory().join("receipt.json"))
                && r.state == State::Stopped
            {
                assert!(r.cleanup_confirmed);
                assert!(!self.manifest.group().exists());
                assert!(!self.manifest.directory().join("run").exists());
                return r;
            }
            assert!(Instant::now() < deadline, "cleanup did not converge");
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    fn populated(&self) -> bool {
        fs::read_to_string(self.manifest.group().join("cgroup.events"))
            .is_ok_and(|s| s.lines().any(|l| l == "populated 1"))
    }
    fn wait_empty(&self) {
        let deadline = Instant::now() + Duration::from_secs(8);
        while self.populated() {
            assert!(
                Instant::now() < deadline,
                "VM descendants survived guardian death/expiry"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = guardian::control(&self.manifest, Action::Stop);
        for _ in 0..350 {
            if self.manifest.reconcile("test_cleanup").is_ok() {
                return;
            }
            if !self.manifest.directory().exists() {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        self.temp.disable_cleanup(true);
        eprintln!(
            "guardian test cleanup unconfirmed; retained {:?}",
            self.temp.path()
        );
    }
}

#[test]
#[ignore = "requires root, HUDSON_GUARDIAN_TEST_VM=1 and verified aarch64 Firecracker artifacts"]
fn real_vm_stop_renewal_fences_and_exact_retry() {
    let f = Fixture::new(5000);
    let mut child = f.spawn();
    let running = f.running(&mut child);
    assert!(f.populated());
    assert!(f.manifest.reconcile("live_recovery_must_fail").is_err());
    assert_eq!(
        fs::read_to_string(f.manifest.group().join("cpu.max"))
            .unwrap()
            .trim(),
        "100000 100000"
    );
    assert_eq!(
        fs::read_to_string(f.manifest.group().join("memory.max"))
            .unwrap()
            .trim(),
        "268435456"
    );
    assert_eq!(
        fs::read_to_string(f.manifest.group().join("pids.max"))
            .unwrap()
            .trim(),
        "64"
    );
    assert_eq!(
        fs::metadata(f.manifest.jail_root().join("rootfs.ext4"))
            .unwrap()
            .len(),
        64 << 20
    );
    let repeated = Command::new(env!("CARGO_BIN_EXE_sandbox-supervisor"))
        .arg("--manifest")
        .arg(&f.path)
        .arg("run")
        .output()
        .unwrap();
    assert!(
        repeated.status.success(),
        "{}",
        String::from_utf8_lossy(&repeated.stderr)
    );
    let receipt: Receipt = serde_json::from_slice(&repeated.stdout).unwrap();
    assert_eq!(receipt.guardian_host_pid, running.guardian_host_pid);
    let until = guardian::wall_ms() + 8000;
    let renewed = guardian::control(
        &f.manifest,
        Action::Renew {
            revision: 1,
            expires_unix_ms: until,
        },
    )
    .unwrap();
    assert!(renewed.error.is_none());
    assert_eq!(renewed.receipt.unwrap().expires_unix_ms, until);
    assert!(
        guardian::control(
            &f.manifest,
            Action::Renew {
                revision: 1,
                expires_unix_ms: until
            }
        )
        .unwrap()
        .error
        .is_none()
    );
    assert!(
        guardian::control(
            &f.manifest,
            Action::Renew {
                revision: 1,
                expires_unix_ms: until + 1000
            }
        )
        .unwrap()
        .error
        .is_some()
    );
    assert!(
        guardian::control(
            &f.manifest,
            Action::Renew {
                revision: 0,
                expires_unix_ms: until
            }
        )
        .unwrap()
        .error
        .is_some()
    );
    let mut wrong = f.manifest.clone();
    wrong.start.owner.project = ProjectId::generate();
    assert!(
        guardian::control(&wrong, Action::Stop)
            .unwrap()
            .error
            .is_some()
    );
    let wait = (f.manifest.start.expires_unix_ms - guardian::wall_ms() + 100).max(0) as u64;
    std::thread::sleep(Duration::from_millis(wait));
    assert!(f.populated(), "renewed VM died at original deadline");
    let _ = guardian::control(&f.manifest, Action::Stop);
    assert!(child.wait().unwrap().success());
    let stopped = f.stopped();
    assert_eq!(stopped.reason.as_deref(), Some("guardian_stopped"));
    assert_eq!(f.manifest.prepare().unwrap().state, State::Stopped);
    assert!(
        guardian::control(
            &f.manifest,
            Action::Renew {
                revision: 2,
                expires_unix_ms: guardian::wall_ms() + 10000
            }
        )
        .is_err()
    );
}
#[test]
#[ignore = "requires root, HUDSON_GUARDIAN_TEST_VM=1 and verified aarch64 Firecracker artifacts"]
fn supervisor_loss_does_not_disable_lease_expiry() {
    let f = Fixture::new(4000);
    let mut child = f.spawn();
    f.running(&mut child);
    child.kill().unwrap();
    child.wait().unwrap();
    std::thread::sleep(Duration::from_millis(100));
    assert!(f.populated());
    f.wait_empty();
    assert!(
        f.manifest.group().exists(),
        "test must observe kernel cleanup before reconciliation"
    );
    let r = f
        .manifest
        .reconcile("lost_supervisor_after_expiry")
        .unwrap();
    assert_eq!(r.state, State::Stopped);
    f.stopped();
}
#[test]
#[ignore = "requires root, HUDSON_GUARDIAN_TEST_VM=1 and verified aarch64 Firecracker artifacts"]
fn killing_namespace_guardian_kills_vm_without_a_surviving_supervisor() {
    let f = Fixture::new(20000);
    let mut child = f.spawn();
    let r = f.running(&mut child);
    child.kill().unwrap();
    child.wait().unwrap();
    let pid = r.guardian_host_pid.unwrap();
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
    let ticks = stat
        .rsplit_once(") ")
        .unwrap()
        .1
        .split_whitespace()
        .nth(19)
        .unwrap()
        .parse::<u64>()
        .unwrap();
    assert_eq!(Some(ticks), r.guardian_start_ticks);
    assert_eq!(
        fs::read_link(format!("/proc/{pid}/exe")).unwrap(),
        PathBuf::from(env!("CARGO_BIN_EXE_sandbox-supervisor"))
    );
    rustix::process::kill_process(
        rustix::process::Pid::from_raw(pid as i32).unwrap(),
        rustix::process::Signal::KILL,
    )
    .unwrap();
    f.wait_empty();
    assert!(f.manifest.group().exists());
    assert_eq!(
        f.manifest.reconcile("guardian_sigkill").unwrap().state,
        State::Stopped
    );
    f.stopped();
}
#[test]
#[ignore = "requires root, HUDSON_GUARDIAN_TEST_VM=1 and verified aarch64 Firecracker artifacts"]
fn staging_digest_failure_and_recovery_fence_prevent_delayed_launch() {
    let mut f = Fixture::new(20000);
    f.manifest.config.rootfs.sha256 = "0".repeat(64);
    assert!(f.manifest.prepare().is_err());
    assert_eq!(f.record().state, State::Stopped);
    assert!(!f.manifest.directory().join("run").exists());
    let f = Fixture::new(20000);
    assert_eq!(f.manifest.prepare().unwrap().state, State::Prepared);
    assert_eq!(
        f.manifest
            .reconcile("recovery_before_launch")
            .unwrap()
            .state,
        State::Stopped
    );
    let mut child = f.spawn();
    assert!(child.wait().unwrap().success());
    assert!(f.record().guardian_host_pid.is_none());
    assert!(!f.manifest.group().exists());
}

#[test]
#[ignore = "requires root, HUDSON_GUARDIAN_TEST_VM=1 and verified aarch64 Firecracker artifacts"]
fn stalled_control_clients_do_not_disable_expiry() {
    use std::io::Write;
    use std::os::unix::net::UnixStream;
    let f = Fixture::new(4000);
    let mut child = f.spawn();
    f.running(&mut child);
    let mut clients = Vec::new();
    for _ in 0..8 {
        let mut client = UnixStream::connect(f.manifest.socket()).unwrap();
        client.write_all(&65536u32.to_be_bytes()).unwrap();
        clients.push(client);
    }
    // Retaining incomplete frames cannot extend the host deadline. The outer
    // wrapper is still present here and must persist expiry plus cleanup.
    assert!(child.wait().unwrap().success());
    let stopped = f.stopped();
    assert_eq!(stopped.reason.as_deref(), Some("lease_expired"));
    assert!(guardian::wall_ms() >= f.manifest.start.expires_unix_ms);
    assert!(guardian::wall_ms() < f.manifest.start.expires_unix_ms + 5000);
}
#[test]
#[ignore = "requires root, HUDSON_GUARDIAN_TEST_VM=1 and verified aarch64 Firecracker artifacts"]
fn concurrent_stop_and_renewal_never_revive_the_vm() {
    let f = Fixture::new(20000);
    let mut child = f.spawn();
    f.running(&mut child);
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
    std::thread::scope(|scope| {
        for action in [
            Action::Stop,
            Action::Renew {
                revision: 1,
                expires_unix_ms: guardian::wall_ms() + 30000,
            },
        ] {
            let barrier = barrier.clone();
            let manifest = &f.manifest;
            scope.spawn(move || {
                barrier.wait();
                // Transport loss after stop is uncertain, never proof of renewal.
                let _ = guardian::control(manifest, action);
            });
        }
        barrier.wait();
    });
    f.stopped();
    assert!(child.wait().unwrap().success());
    assert_eq!(f.manifest.prepare().unwrap().state, State::Stopped);
}
#[test]
#[ignore = "requires root, HUDSON_GUARDIAN_TEST_VM=1 and verified aarch64 Firecracker artifacts"]
fn recovery_of_launch_intent_fences_before_any_new_spawn() {
    let f = Fixture::new(20000);
    let mut receipt = f.manifest.prepare().unwrap();
    receipt.state = State::LaunchIntent;
    // Simulate a crash after durable intent, before starting the jailer.
    fs::write(
        f.manifest.directory().join("receipt.json"),
        serde_json::to_vec(&receipt).unwrap(),
    )
    .unwrap();
    let mut child = f.spawn();
    assert!(child.wait().unwrap().success());
    let stopped = f.stopped();
    assert!(stopped.guardian_host_pid.is_none());
    assert_eq!(stopped.reason.as_deref(), Some("guardian_recovery_fence"));
}

#[test]
#[ignore = "requires root, HUDSON_GUARDIAN_TEST_VM=1, release guest binary and aarch64 artifacts"]
fn bootstrapped_guest_executes_only_after_durable_binding() {
    use sandbox_protocol::guest::Stream;
    use sandbox_protocol::guest_model::Execute;
    let f = Fixture::build(30000, true);
    let source_digest = f.manifest.config.rootfs.sha256.clone();
    let prepared = f.manifest.prepare().unwrap();
    assert_eq!(
        f.manifest.prepare().unwrap().identity_digest,
        prepared.identity_digest
    );
    let mut child = f.spawn();
    let running = f.running(&mut child);
    assert!(running.guest_boot_id.is_none());
    assert!(f.manifest.guest_client().is_err());
    let ready = f.ready();
    assert_eq!(f.record().guest_boot_id, ready.guest_boot_id);
    assert_eq!(
        guardian::control(&f.manifest, Action::BindGuest)
            .unwrap()
            .receipt
            .unwrap()
            .guest_boot_id,
        ready.guest_boot_id
    );
    assert_eq!(f.record().identity_digest, running.identity_digest);
    assert_eq!(
        artifact(f.manifest.config.rootfs.path.clone()).sha256,
        source_digest
    );
    let client = f.manifest.guest_client().unwrap();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let request = Execute {
            operation_id: OperationId::generate(),
            argv: vec![
                "/bin/busybox".into(),
                "sh".into(),
                "-c".into(),
                "if printf nope >/dev/vdb; then exit 99; fi; echo once >> /root/once; /bin/busybox id -u; /bin/busybox cat /root/once; exit 9"
                    .into(),
            ],
            env: Default::default(),
            cwd: "/root".into(),
            deadline_unix_ms: guardian::wall_ms() + 10000,
            output_limit: 4096,
        };
        client.execute(&request).await.unwrap();
        let until = Instant::now() + Duration::from_secs(8);
        let receipt = loop {
            let receipt = client.inspect(request.operation_id).await.unwrap();
            if receipt.state.terminal() {
                break receipt;
            }
            assert!(Instant::now() < until);
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
        assert_eq!(receipt.exit_code, Some(9));
        assert!(receipt.cleanup_confirmed);
        assert_eq!(client.execute(&request).await.unwrap().exit_code, Some(9));
        let output = client
            .output(sandbox_protocol::guest::ReadOutput {
                operation_id: request.operation_id.to_string(),
                stream: Stream::Stdout as i32,
                offset: 0,
                limit: 4096,
            })
            .await
            .unwrap();
        assert_eq!(output.data, b"0\nonce\n");
        assert!(output.complete);
        let mut changed = request.clone();
        changed.argv.push("different".into());
        assert!(client.execute(&changed).await.is_err());
    });
    let _ = guardian::control(&f.manifest, Action::Stop);
    assert!(child.wait().unwrap().success());
    f.stopped();
    assert!(f.manifest.guest_client().is_err());
}
#[test]
#[ignore = "requires root, HUDSON_GUARDIAN_TEST_VM=1 and verified aarch64 Firecracker artifacts"]
fn changed_bootstrap_is_fenced_before_launch() {
    let f = Fixture::new(20000);
    let prepared = f.manifest.prepare().unwrap();
    assert!(prepared.identity_digest.is_some());
    let path = f.manifest.jail_root().join("bootstrap.img");
    let mut changed = fs::read(&path).unwrap();
    changed[16] ^= 1; // Preserve device length; exercise content verification.
    fs::write(path, changed).unwrap();
    let mut child = f.spawn();
    assert!(child.wait().unwrap().success());
    let stopped = f.stopped();
    assert!(stopped.firecracker_namespace_pid.is_none());
}

#[test]
#[ignore = "requires root, HUDSON_GUARDIAN_TEST_VM=1, release guest binary and aarch64 artifacts"]
fn shared_base_image_keeps_allocation_credentials_separate() {
    use sandbox_supervisor::{guest::GuestClient, identity::Identity};
    let first = Fixture::build(30000, true);
    let mut first_process = first.spawn();
    first.running(&mut first_process);
    let first_ready = first.ready();
    let first_identity: Identity =
        guardian::read_json(&first.manifest.directory().join("run/identity.json")).unwrap();
    let mut second = Fixture::new(30000);
    second.manifest.config.rootfs = first.manifest.config.rootfs.clone();
    fs::write(&second.path, serde_json::to_vec(&second.manifest).unwrap()).unwrap();
    let mut second_process = second.spawn();
    second.running(&mut second_process);
    let second_ready = second.ready();
    assert_ne!(first_ready.identity_digest, second_ready.identity_digest);
    assert_ne!(first_ready.guest_boot_id, second_ready.guest_boot_id);
    assert_eq!(
        first.manifest.config.rootfs.sha256,
        second.manifest.config.rootfs.sha256
    );
    let context = sandbox_protocol::guest_model::Context {
        allocation_id: second.manifest.start.owner.allocation,
        generation: 1,
        boot_id: second_ready.guest_boot_id.unwrap(),
    };
    let wrong = GuestClient::from_firecracker_directory(
        second.manifest.jail_root(),
        52,
        first_identity.client_tls(guardian::wall_ms()).unwrap(),
        context.clone(),
    )
    .unwrap();
    let right = second.manifest.guest_client().unwrap();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            assert!(wrong.hello().await.is_err());
            assert_eq!(right.hello().await.unwrap(), context);
        });
    let _ = guardian::control(&second.manifest, Action::Stop);
    assert!(second_process.wait().unwrap().success());
    second.stopped();
    assert!(first.populated());
    let _ = guardian::control(&first.manifest, Action::Stop);
    assert!(first_process.wait().unwrap().success());
    first.stopped();
}
