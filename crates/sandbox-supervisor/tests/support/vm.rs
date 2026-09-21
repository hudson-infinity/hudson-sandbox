//! Shared controlled Firecracker VM fixture.
#![allow(dead_code)]
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

pub(super) struct Fixture {
    pub(super) temp: tempfile::TempDir,
    pub(super) manifest: Manifest,
    pub(super) path: PathBuf,
}
pub(super) fn artifact(path: PathBuf) -> Artifact {
    Artifact {
        sha256: hex::encode(Sha256::digest(fs::read(&path).unwrap())),
        path,
    }
}
impl Fixture {
    pub(super) fn new(ttl_ms: i64) -> Self {
        Self::build(ttl_ms, false)
    }
    pub(super) fn build(ttl_ms: i64, agent: bool) -> Self {
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
    pub(super) fn spawn(&self) -> Child {
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
    pub(super) fn record(&self) -> Receipt {
        guardian::read_json(&self.manifest.directory().join("receipt.json")).unwrap()
    }
    pub(super) fn running(&self, child: &mut Child) -> Receipt {
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
    pub(super) fn ready(&self) -> Receipt {
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
    pub(super) fn stopped(&self) -> Receipt {
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
    pub(super) fn populated(&self) -> bool {
        fs::read_to_string(self.manifest.group().join("cgroup.events"))
            .is_ok_and(|s| s.lines().any(|l| l == "populated 1"))
    }
    pub(super) fn wait_empty(&self) {
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
