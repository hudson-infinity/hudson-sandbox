//! Root-owned Linux allocation staging, fencing and cleanup. No customer host commands.
mod process;
use anyhow::{Context as _, Result, ensure};
pub use process::{control, launch, namespace_init};
use sandbox_protocol::{AllocationId, HostId, Id, OperationId, ProjectId, SandboxId};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Owner {
    pub host: HostId,
    pub project: ProjectId,
    pub sandbox: SandboxId,
    pub allocation: AllocationId,
    pub create_operation: OperationId,
    pub generation: i64,
    pub epoch: i64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Artifact {
    pub path: PathBuf,
    pub sha256: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub state_root: PathBuf,
    pub cgroup_parent: PathBuf,
    pub firecracker: Artifact,
    pub jailer: Artifact,
    pub kernel: Artifact,
    pub rootfs: Artifact,
    pub jail_uid: u32,
    pub jail_gid: u32,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Start {
    pub owner: Owner,
    pub vcpu: u32,
    pub memory_mib: u64,
    pub disk_mib: u64,
    pub expires_unix_ms: i64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub config: Config,
    pub start: Start,
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Staging,
    Prepared,
    LaunchIntent,
    Running,
    Stopping,
    Fenced,
    Stopped,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Receipt {
    pub version: u32,
    pub owner: Owner,
    pub digest: String,
    pub host_boot_id: String,
    pub state: State,
    pub cleanup_confirmed: bool,
    pub renewal_revision: u64,
    pub expires_unix_ms: i64,
    pub guardian_host_pid: Option<u32>,
    pub guardian_start_ticks: Option<u64>,
    pub firecracker_namespace_pid: Option<u32>,
    pub cgroup_inode: Option<u64>,
    pub reason: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum Action {
    Inspect,
    Renew { revision: u64, expires_unix_ms: i64 },
    Stop,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub owner: Owner,
    pub action: Action,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Response {
    pub receipt: Option<Receipt>,
    pub error: Option<String>,
}

#[derive(Debug, thiserror::Error)]
#[error("allocation guardian is still active")]
struct OwnershipBusy;

pub fn wall_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|t| i64::try_from(t.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}
pub fn boot_ms() -> u64 {
    let t = rustix::time::clock_gettime(rustix::time::ClockId::Boottime);
    (t.tv_sec as u64)
        .saturating_mul(1000)
        .saturating_add((t.tv_nsec as u64) / 1_000_000)
}
fn boot_id() -> Result<String> {
    Ok(fs::read_to_string("/proc/sys/kernel/random/boot_id")?
        .trim()
        .into())
}
fn root() -> Result<()> {
    ensure!(
        rustix::process::geteuid().is_root(),
        "allocation guardian requires root"
    );
    Ok(())
}
pub fn read_json<T: DeserializeOwned>(path: &Path) -> Result<T> {
    let mut data = Vec::new();
    let file = OpenOptions::new()
        .read(true)
        .custom_flags((rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK).bits() as i32)
        .open(path)?;
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file() && metadata.len() <= 65536,
        "invalid guardian metadata file"
    );
    file.take(65537).read_to_end(&mut data)?;
    ensure!(data.len() <= 65536, "guardian metadata too large");
    serde_json::from_slice(&data).map_err(|_| anyhow::anyhow!("invalid guardian metadata"))
}
fn write_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let parent = path.parent().context("missing metadata parent")?;
    let temp = parent.join(format!("{}.tmp", OperationId::generate()));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temp)?;
    let bytes = serde_json::to_vec(value)?;
    ensure!(bytes.len() <= 65536, "guardian metadata too large");
    file.write_all(&bytes)?;
    file.sync_all()?;
    fs::rename(&temp, path)?;
    File::open(parent)?.sync_all()?;
    Ok(())
}
fn private_dir(path: &Path) -> Result<()> {
    match fs::DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e.into()),
    }
    let m = fs::symlink_metadata(path)?;
    ensure!(
        m.is_dir() && m.uid() == 0 && m.mode() & 0o077 == 0,
        "guardian directory must be private and root-owned"
    );
    Ok(())
}
impl Manifest {
    pub fn directory(&self) -> PathBuf {
        self.config
            .state_root
            .join(self.start.owner.allocation.to_string())
    }
    pub fn group(&self) -> PathBuf {
        self.config
            .cgroup_parent
            .join(self.start.owner.allocation.uuid().to_string())
    }
    pub fn jail_root(&self) -> PathBuf {
        self.directory()
            .join("run/chroots/firecracker")
            .join(self.start.owner.allocation.uuid().to_string())
            .join("root")
    }
    pub fn socket(&self) -> PathBuf {
        self.directory().join("control.sock")
    }
    fn record_path(&self) -> PathBuf {
        self.directory().join("receipt.json")
    }
    fn digest(&self) -> Result<String> {
        Ok(hex::encode(Sha256::digest(serde_json::to_vec(self)?)))
    }
    fn validate(&self) -> Result<()> {
        root()?;
        ensure!(
            self.start.owner.generation > 0 && self.start.owner.epoch > 0,
            "invalid allocation ownership"
        );
        ensure!(
            (1..=4).contains(&self.start.vcpu)
                && (128..=8192).contains(&self.start.memory_mib)
                && (64..=65536).contains(&self.start.disk_mib),
            "invalid VM resource bounds"
        );
        ensure!(
            self.config.jail_uid >= 65534 && self.config.jail_gid >= 65534,
            "jailer must use an unprivileged UID/GID"
        );
        ensure!(
            self.config.state_root.is_absolute()
                && self
                    .config
                    .state_root
                    .components()
                    .all(|c| !matches!(c, std::path::Component::ParentDir)),
            "invalid state root"
        );
        ensure!(
            self.config.cgroup_parent.starts_with("/sys/fs/cgroup/")
                && self.config.cgroup_parent != Path::new("/sys/fs/cgroup")
                && self
                    .config
                    .cgroup_parent
                    .components()
                    .all(|c| !matches!(c, std::path::Component::ParentDir)),
            "dedicated cgroup parent required"
        );
        ensure!(
            self.socket().as_os_str().len() < 100,
            "guardian Unix socket path too long"
        );
        for artifact in [
            &self.config.firecracker,
            &self.config.jailer,
            &self.config.kernel,
            &self.config.rootfs,
        ] {
            ensure!(
                artifact.path.is_absolute()
                    && artifact.sha256.len() == 64
                    && artifact
                        .sha256
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
                "invalid artifact path or digest"
            );
        }
        ensure!(
            serde_json::to_vec(self)?.len() <= 65536,
            "manifest too large"
        );
        Ok(())
    }
    fn lock(&self) -> Result<File> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32)
            .open(self.directory().join("lifecycle.lock"))?;
        match rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive) {
            Ok(()) => {}
            Err(rustix::io::Errno::WOULDBLOCK) => return Err(OwnershipBusy.into()),
            Err(e) => return Err(e.into()),
        }
        Ok(file)
    }
    fn receipt(&self) -> Result<Receipt> {
        let r: Receipt = read_json(&self.record_path())?;
        ensure!(
            r.version == 1
                && r.owner == self.start.owner
                && r.digest == self.digest()?
                && r.reason.as_ref().is_none_or(|s| s.len() < 256)
                && r.cleanup_confirmed == (r.state == State::Stopped),
            "guardian receipt ownership or shape mismatch"
        );
        Ok(r)
    }
    /// Stage once under ownership. A returned existing receipt never causes a second launch.
    pub fn prepare(&self) -> Result<Receipt> {
        self.validate()?;
        private_dir(&self.config.state_root)?;
        private_dir(&self.directory())?;
        let _lock = self.lock()?;
        if self.record_path().exists() {
            return self.receipt();
        }
        ensure!(
            !self.group().exists(),
            "unowned allocation cgroup already exists"
        );
        ensure!(
            fs::read_dir(self.directory())?.count() == 1,
            "unowned allocation files already exist"
        );
        crate::lease::Deadline::new(self.start.expires_unix_ms, wall_ms(), boot_ms())?;
        let mut record = Receipt {
            version: 1,
            owner: self.start.owner.clone(),
            digest: self.digest()?,
            host_boot_id: boot_id()?,
            state: State::Staging,
            cleanup_confirmed: false,
            renewal_revision: 0,
            expires_unix_ms: self.start.expires_unix_ms,
            guardian_host_pid: None,
            guardian_start_ticks: None,
            firecracker_namespace_pid: None,
            cgroup_inode: None,
            reason: None,
        };
        write_json(&self.record_path(), &record)?;
        write_json(&self.directory().join("manifest.json"), self)?;
        if self.stage().is_err() {
            record.state = State::Fenced;
            record.reason = Some("artifact_staging_failed".into());
            write_json(&self.record_path(), &record)?;
            self.finish_cleanup(&mut record)?;
            anyhow::bail!("artifact staging failed; allocation fenced")
        }
        record.state = State::Prepared;
        write_json(&self.record_path(), &record)?;
        Ok(record)
    }
    fn stage(&self) -> Result<()> {
        private_dir(&self.directory().join("run"))?;
        let run = self.directory().join("run");
        let jail = self.jail_root();
        fs::create_dir_all(&jail)?;
        fn copy(source: &Artifact, dest: &Path, max: u64, executable: bool) -> Result<()> {
            let mut input = OpenOptions::new()
                .read(true)
                .custom_flags(
                    (rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK).bits() as i32,
                )
                .open(&source.path)?;
            let m = input.metadata()?;
            ensure!(
                m.is_file() && m.len() > 0 && m.len() <= max,
                "artifact outside size bound"
            );
            let mut output = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(dest)?;
            let mut digest = Sha256::new();
            let mut total = 0u64;
            let mut bytes = [0u8; 65536];
            loop {
                let n = input.read(&mut bytes)?;
                if n == 0 {
                    break;
                }
                total += n as u64;
                ensure!(total <= max, "artifact grew past bound");
                digest.update(&bytes[..n]);
                output.write_all(&bytes[..n])?;
            }
            ensure!(
                hex::encode(digest.finalize()) == source.sha256,
                "artifact digest mismatch"
            );
            output.sync_all()?;
            fs::set_permissions(
                dest,
                fs::Permissions::from_mode(if executable { 0o755 } else { 0o600 }),
            )?;
            Ok(())
        }
        copy(
            &self.config.firecracker,
            &run.join("firecracker"),
            128 << 20,
            true,
        )?;
        copy(&self.config.jailer, &run.join("jailer"), 16 << 20, true)?;
        copy(&self.config.kernel, &jail.join("vmlinux"), 256 << 20, false)?;
        let disk_bytes = self.start.disk_mib << 20;
        copy(
            &self.config.rootfs,
            &jail.join("rootfs.ext4"),
            disk_bytes,
            false,
        )?;
        let disk = OpenOptions::new()
            .write(true)
            .open(jail.join("rootfs.ext4"))?;
        disk.set_len(disk_bytes)?;
        rustix::fs::fallocate(&disk, rustix::fs::FallocateFlags::empty(), 0, disk_bytes)?;
        disk.sync_all()?;
        let config = serde_json::json!({"boot-source":{"kernel_image_path":"/vmlinux","boot_args":"keep_bootcon console=ttyS0 reboot=k panic=1 pci=off root=/dev/vda rw init=/init"},"drives":[{"drive_id":"rootfs","path_on_host":"/rootfs.ext4","is_root_device":true,"is_read_only":false}],"machine-config":{"vcpu_count":self.start.vcpu,"mem_size_mib":self.start.memory_mib,"smt":false},"network-interfaces":[],"vsock":{"guest_cid":3,"uds_path":"/vsock.sock"}});
        write_json(&jail.join("config.json"), &config)?;
        for name in ["vmlinux", "rootfs.ext4", "config.json"] {
            std::os::unix::fs::chown(
                jail.join(name),
                Some(self.config.jail_uid),
                Some(self.config.jail_gid),
            )?;
        }
        fs::set_permissions(jail.join("vmlinux"), fs::Permissions::from_mode(0o400))?;
        fs::set_permissions(jail.join("config.json"), fs::Permissions::from_mode(0o400))?;
        std::os::unix::fs::chown(
            &jail,
            Some(self.config.jail_uid),
            Some(self.config.jail_gid),
        )?;
        for file in [
            run.join("firecracker"),
            run.join("jailer"),
            jail.join("vmlinux"),
            jail.join("rootfs.ext4"),
            jail.join("config.json"),
        ] {
            File::open(file)?.sync_all()?;
        }
        let mut directory = jail.as_path();
        loop {
            File::open(directory)?.sync_all()?;
            if directory == self.directory() {
                break;
            }
            directory = directory.parent().context("missing staging parent")?;
        }
        File::open(&run)?.sync_all()?;
        File::open(self.directory())?.sync_all()?;
        Ok(())
    }
    /// Only a free lifecycle lock permits fencing and cleanup. Never signal a saved arbitrary PID.
    pub fn reconcile(&self, reason: &str) -> Result<Receipt> {
        self.validate()?;
        let _lock = self.lock()?;
        let mut r = self.receipt()?;
        if r.state == State::Stopped {
            return Ok(r);
        }
        r.state = State::Fenced;
        r.cleanup_confirmed = false;
        r.reason = Some(reason.into());
        write_json(&self.record_path(), &r)?;
        self.finish_cleanup(&mut r)?;
        Ok(r)
    }
    fn finish_cleanup(&self, r: &mut Receipt) -> Result<()> {
        let group = self.group();
        match fs::metadata(&group) {
            Ok(m) => {
                ensure!(m.is_dir(), "invalid owned cgroup");
                let populated = fs::read_to_string(group.join("cgroup.events"))?
                    .lines()
                    .any(|s| s == "populated 1");
                if populated {
                    ensure!(
                        r.host_boot_id == boot_id()? && r.cgroup_inode == Some(m.ino()),
                        "cgroup ownership cannot be proven"
                    );
                    fs::write(group.join("cgroup.kill"), "1")?;
                }
                let deadline = std::time::Instant::now() + Duration::from_secs(5);
                loop {
                    if fs::read_to_string(group.join("cgroup.events"))?
                        .lines()
                        .any(|s| s == "populated 0")
                    {
                        break;
                    }
                    ensure!(
                        std::time::Instant::now() < deadline,
                        "VM cleanup unconfirmed"
                    );
                    std::thread::sleep(Duration::from_millis(20));
                }
                fs::remove_dir(group)?;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        let run = self.directory().join("run");
        if run.exists() {
            fs::remove_dir_all(run)?;
        }
        match fs::remove_file(self.socket()) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        r.state = State::Stopped;
        r.cleanup_confirmed = true;
        write_json(&self.record_path(), r)?;
        Ok(())
    }
}
