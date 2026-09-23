//! Recoverable removal of a stopped guardian's three owned metadata files.
//! The caller MUST durably retain Plan before calling remove. Never recursive.
use super::*;
use sandbox_protocol::allocation_retirement::Intent;
use std::collections::BTreeMap;

/// Nonblocking flock contention is a transient lifecycle state, not evidence
/// that the retirement request is invalid. Callers may retry only this error.
pub(crate) fn is_lock_contended(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<rustix::io::Errno>()
            .is_some_and(|errno| *errno == rustix::io::Errno::WOULDBLOCK)
            || cause
                .downcast_ref::<std::io::Error>()
                .is_some_and(|error| error.kind() == std::io::ErrorKind::WouldBlock)
    })
}

const NAMES: [&str; 3] = ["receipt.json", "manifest.json", "lifecycle.lock"];
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Identity {
    device: u64,
    inode: u64,
}
impl Identity {
    fn of(m: &fs::Metadata) -> Self {
        Self {
            device: m.dev(),
            inode: m.ino(),
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    identity: Identity,
    len: u64,
    sha256: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Metadata {
    Absent {},
    Stopped {
        directory: Identity,
        receipt: Box<Receipt>,
        files: BTreeMap<String, Entry>,
    },
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Plan {
    version: u32,
    intent_sha256: String,
    metadata: Metadata,
}
impl Plan {
    pub(crate) fn receipt(&self) -> Option<&Receipt> {
        match &self.metadata {
            Metadata::Absent {} => None,
            Metadata::Stopped { receipt, .. } => Some(receipt),
        }
    }
    fn validate(&self, intent: &Intent, manifest: Option<&Manifest>) -> Result<()> {
        ensure!(
            self.version == 1 && self.intent_sha256 == hex::encode(intent.digest()?),
            "deletion scope mismatch"
        );
        match (&self.metadata, manifest) {
            (Metadata::Absent {}, None) => {}
            (
                Metadata::Stopped {
                    directory,
                    receipt,
                    files,
                },
                Some(manifest),
            ) => {
                manifest.validate_receipt(receipt)?;
                ensure!(
                    receipt.state == State::Stopped && receipt.cleanup_confirmed,
                    "unverified deletion cleanup"
                );
                ensure!(
                    files.len() == NAMES.len() && NAMES.iter().all(|n| files.contains_key(*n)),
                    "invalid deletion inventory"
                );
                ensure!(directory.inode > 0, "invalid deletion directory identity");
                let receipt_bytes = serde_json::to_vec(receipt)?;
                let manifest_bytes = serde_json::to_vec(manifest)?;
                for (name, bytes) in [
                    ("receipt.json", receipt_bytes),
                    ("manifest.json", manifest_bytes),
                    ("lifecycle.lock", Vec::new()),
                ] {
                    ensure!(
                        files[name].len == bytes.len() as u64
                            && files[name].sha256 == hex::encode(Sha256::digest(bytes)),
                        "retained deletion evidence differs from original file digest"
                    );
                }
                for (name, file) in files {
                    ensure!(
                        file.identity.inode > 0
                            && file.len <= 65536
                            && file.sha256.len() == 64
                            && file
                                .sha256
                                .bytes()
                                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
                        "invalid deletion file digest"
                    );
                    ensure!(
                        name != "lifecycle.lock" || file.len == 0,
                        "invalid lifecycle lock contents"
                    );
                }
            }
            _ => anyhow::bail!("deletion manifest mismatch"),
        }
        Ok(())
    }
}
fn absent(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
        Ok(_) => anyhow::bail!("retirement resource still present"),
    }
}
/// Absence under independently held root authority. This supplies no proof on
/// its own; Closed serials must never turn it into an original-owner receipt.
pub(crate) fn verify_absent(root: &Path, cgroup_parent: &Path, intent: &Intent) -> Result<()> {
    intent.validate()?;
    ensure!(!intent.simulated, "physical retirement required");
    ensure!(
        cgroup_parent.starts_with("/sys/fs/cgroup/")
            && cgroup_parent != Path::new("/sys/fs/cgroup")
            && !cgroup_parent
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir)),
        "invalid retirement cgroup parent"
    );
    absent(&root.join(intent.permit.allocation.to_string()))?;
    absent(&cgroup_parent.join(intent.permit.allocation.uuid().to_string()))?;
    File::open(root)?.sync_all()?;
    Ok(())
}
impl Plan {
    pub(crate) fn verify_removed(
        &self,
        root: &Path,
        cgroup_parent: &Path,
        intent: &Intent,
        manifest: Option<&Manifest>,
    ) -> Result<()> {
        self.validate(intent, manifest)?;
        if let Some(m) = manifest {
            m.validate()?;
            ensure!(
                m.launch_permit.as_ref() == Some(&intent.permit)
                    && m.config.state_root == root
                    && m.config.cgroup_parent == cgroup_parent,
                "retirement guardian scope mismatch"
            );
        }
        verify_absent(root, cgroup_parent, intent)
    }
}
fn entry(path: &Path) -> Result<Entry> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags((rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK).bits() as i32)
        .open(path)?;
    let m = file.metadata()?;
    ensure!(
        m.is_file() && m.uid() == 0 && m.mode() & 0o077 == 0 && m.nlink() == 1 && m.len() <= 65536,
        "unowned retirement metadata"
    );
    let mut bytes = Vec::new();
    file.take(65537).read_to_end(&mut bytes)?;
    ensure!(bytes.len() as u64 == m.len(), "retirement file changed");
    Ok(Entry {
        identity: Identity::of(&m),
        len: m.len(),
        sha256: hex::encode(Sha256::digest(bytes)),
    })
}
fn same_entry(path: &Path, expected: &Entry) -> Result<()> {
    let actual = entry(path)?;
    ensure!(
        actual.identity == expected.identity
            && actual.len == expected.len
            && actual.sha256 == expected.sha256,
        "retirement metadata replaced or changed"
    );
    Ok(())
}
/// Holds the persistent exclusive authority lock and the original lifecycle lock.
/// Neither lock can be replaced or recreated to authorize this deletion.
pub(crate) struct Session {
    _authority: crate::launch_authority::LaunchGuard,
    _lifecycle: Option<File>,
    directory: Option<File>,
    path: PathBuf,
    root: PathBuf,
    pub(crate) plan: Plan,
}
impl Session {
    pub(crate) fn open(
        root: &Path,
        cgroup_parent: &Path,
        epoch: i64,
        frontier: u64,
        intent: &Intent,
        manifest: Option<&Manifest>,
        saved: Option<&Plan>,
    ) -> Result<Self> {
        let authority = crate::launch_authority::authorize_deletion(root, epoch, frontier, intent)?;
        ensure!(
            cgroup_parent.starts_with("/sys/fs/cgroup/")
                && cgroup_parent != Path::new("/sys/fs/cgroup")
                && !cgroup_parent
                    .components()
                    .any(|c| matches!(c, std::path::Component::ParentDir)),
            "invalid retirement cgroup parent"
        );
        let p = &intent.permit;
        let path = root.join(p.allocation.to_string());
        absent(&cgroup_parent.join(p.allocation.uuid().to_string()))?;
        if let Some(m) = manifest {
            m.validate()?;
            ensure!(
                m.launch_permit.as_ref() == Some(p)
                    && m.config.state_root == root
                    && m.config.cgroup_parent == cgroup_parent,
                "retirement guardian scope mismatch"
            );
        }
        if let Some(plan) = saved {
            plan.validate(intent, manifest)?;
        }
        let directory = match fs::symlink_metadata(&path) {
            Ok(m) => {
                ensure!(
                    m.is_dir()
                        && m.uid() == 0
                        && m.mode() & 0o077 == 0
                        && fs::canonicalize(&path)? == path,
                    "unowned retirement directory"
                );
                if let Some(plan) = saved {
                    match &plan.metadata {
                        Metadata::Stopped { directory, .. } => ensure!(
                            Identity::of(&m) == *directory,
                            "retirement directory replaced"
                        ),
                        Metadata::Absent {} => {
                            anyhow::bail!("unexpected directory for unused permit")
                        }
                    }
                }
                Some(File::open(&path)?)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                ensure!(
                    saved.is_some() || manifest.is_none(),
                    "missing original guardian metadata"
                );
                None
            }
            Err(e) => return Err(e.into()),
        };
        let mut lifecycle = None;
        let mut actual = BTreeMap::new();
        if directory.is_some() {
            ensure!(manifest.is_some(), "unknown allocation directory");
            let lock_path = path.join("lifecycle.lock");
            match OpenOptions::new()
                .read(true)
                .write(true)
                .custom_flags(
                    (rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK).bits() as i32,
                )
                .open(&lock_path)
            {
                Ok(file) => {
                    // Validate type/ownership before flock; do not block on FIFOs or devices.
                    let identity = entry(&lock_path)?;
                    ensure!(
                        identity.len == 0 && Identity::of(&file.metadata()?) == identity.identity,
                        "invalid lifecycle identity"
                    );
                    rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive)?;
                    lifecycle = Some(file);
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound && saved.is_some() => {}
                Err(e) => return Err(e.into()),
            }
            for item in fs::read_dir(&path)? {
                let item = item?;
                let name = item
                    .file_name()
                    .into_string()
                    .map_err(|_| anyhow::anyhow!("invalid metadata name"))?;
                ensure!(
                    NAMES.contains(&name.as_str()),
                    "unknown allocation metadata prevents deletion"
                );
                actual.insert(name, entry(&item.path())?);
            }
            ensure!(
                lifecycle.is_some() || actual.is_empty(),
                "missing lifecycle lock before metadata removal"
            );
            if let Some(plan) = saved {
                if let Metadata::Stopped { files, .. } = &plan.metadata {
                    for name in actual.keys() {
                        same_entry(&path.join(name), &files[name])?;
                    }
                }
            } else {
                ensure!(
                    actual.len() == NAMES.len(),
                    "incomplete original guardian metadata"
                );
            }
        }
        let plan = if let Some(plan) = saved {
            plan.clone()
        } else {
            let metadata = if let Some(manifest) = manifest {
                let disk: Manifest = read_json(&path.join("manifest.json"))?;
                ensure!(
                    disk.digest()? == manifest.digest()?,
                    "retirement manifest changed"
                );
                let receipt = manifest.receipt()?;
                ensure!(
                    receipt.cleanup_confirmed && receipt.state == State::Stopped,
                    "guardian cleanup unconfirmed"
                );
                Metadata::Stopped {
                    directory: Identity::of(
                        &directory
                            .as_ref()
                            .context("missing directory")?
                            .metadata()?,
                    ),
                    receipt: Box::new(receipt),
                    files: actual,
                }
            } else {
                Metadata::Absent {}
            };
            Plan {
                version: 1,
                intent_sha256: hex::encode(intent.digest()?),
                metadata,
            }
        };
        plan.validate(intent, manifest)?;
        Ok(Self {
            _authority: authority,
            _lifecycle: lifecycle,
            directory,
            path,
            root: root.into(),
            plan,
        })
    }
    pub(crate) fn is_removed(&self) -> bool {
        self.directory.is_none()
    }
    /// One bounded, synced deletion step; safe to restart from the retained plan.
    pub(crate) fn remove_next(&mut self) -> Result<bool> {
        if let Some(dir) = &self.directory {
            if let Metadata::Stopped { files, .. } = &self.plan.metadata {
                for name in NAMES {
                    match fs::symlink_metadata(self.path.join(name)) {
                        Ok(_) => {
                            same_entry(&self.path.join(name), &files[name])?;
                            fs::remove_file(self.path.join(name))?;
                            dir.sync_all()?;
                            return Ok(false);
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                        Err(e) => return Err(e.into()),
                    }
                }
            }
            fs::remove_dir(&self.path)?;
            self.directory = None;
        }
        File::open(&self.root)?.sync_all()?;
        Ok(true)
    }
}

#[cfg(test)]
#[path = "retirement_tests.rs"]
mod tests;
