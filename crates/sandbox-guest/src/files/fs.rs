use anyhow::{Context as _, Result, ensure};
use rustix::fs::{self as rfs, Mode, OFlags, ResolveFlags};
use sandbox_protocol::{Id, OperationId, files::*};
use serde::{Serialize, de::DeserializeOwned};
use std::{
    fs::{File, OpenOptions},
    io::{Read, Write},
    os::{
        fd::AsRawFd,
        unix::fs::{MetadataExt, OpenOptionsExt},
    },
    path::Path,
    sync::atomic::AtomicUsize,
};

const RESOLVE: ResolveFlags = ResolveFlags::BENEATH
    .union(ResolveFlags::NO_SYMLINKS)
    .union(ResolveFlags::NO_XDEV);
const READ: OFlags = OFlags::RDONLY.union(OFlags::CLOEXEC);
const DIRECTORY: OFlags = READ.union(OFlags::DIRECTORY);

pub(super) struct Root {
    root: File,
    pub(super) state: File,
    _lock: File,
    pub(super) downloads: AtomicUsize,
}
impl Root {
    pub(super) fn open(path: &Path) -> Result<Self> {
        ensure!(path.is_absolute(), "workspace root must be absolute");
        let root = OpenOptions::new()
            .read(true)
            .custom_flags((OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC).bits() as i32)
            .open(path)?;
        match rfs::mkdirat(&root, STATE_DIRECTORY, Mode::from_bits_retain(0o700)) {
            Ok(()) => root.sync_all()?,
            Err(rustix::io::Errno::EXIST) => {}
            Err(error) => return Err(error.into()),
        }
        let state = File::from(rfs::openat2(
            &root,
            STATE_DIRECTORY,
            DIRECTORY,
            Mode::empty(),
            RESOLVE,
        )?);
        let lock = match create(&state, "lock") {
            Ok(file) => {
                file.sync_all()?;
                state.sync_all()?;
                file
            }
            Err(error)
                if error.downcast_ref::<rustix::io::Errno>() == Some(&rustix::io::Errno::EXIST) =>
            {
                regular(&state, "lock", true)?
            }
            Err(error) => return Err(error),
        };
        rfs::flock(&lock, rfs::FlockOperation::NonBlockingLockExclusive)
            .context("file workspace already owned")?;
        Ok(Self {
            root,
            state,
            _lock: lock,
            downloads: AtomicUsize::new(0),
        })
    }
    pub(super) fn names(&self) -> Result<Vec<String>> {
        let mut names = Vec::new();
        for entry in rfs::Dir::read_from(&self.state)? {
            let entry = entry?;
            let name = entry.file_name().to_str()?.to_owned();
            if name == "." || name == ".." {
                continue;
            }
            ensure!(
                names.len() < MAX_TRANSFERS * 3 + 3,
                "too many file state entries"
            );
            names.push(name);
        }
        Ok(names)
    }
    pub(super) fn parent(&self, path: &str) -> Result<(File, String)> {
        validate_path(path)?;
        let (parent, name) = path.rsplit_once('/').unwrap_or((".", path));
        Ok((
            File::from(rfs::openat2(
                &self.root,
                parent,
                DIRECTORY,
                Mode::empty(),
                RESOLVE,
            )?),
            name.to_owned(),
        ))
    }
    pub(super) fn stage(&self, id: OperationId, new: bool) -> Result<File> {
        let name = format!("{id}.data");
        if new {
            create(&self.state, &name)
        } else {
            regular(&self.state, &name, true)
        }
    }
    pub(super) fn remove(&self, name: &str) -> Result<()> {
        match rfs::unlinkat(&self.state, name, rfs::AtFlags::empty()) {
            Ok(()) => self.state.sync_all()?,
            Err(rustix::io::Errno::NOENT) => {}
            Err(error) => return Err(error.into()),
        }
        Ok(())
    }
    pub(super) fn publish(&self, id: OperationId, parent: &File, name: &str) -> Result<()> {
        rfs::renameat(&self.state, format!("{id}.data"), parent, name)?;
        parent.sync_all()?;
        self.state.sync_all()?;
        Ok(())
    }
    pub(super) fn read(&self, path: &str) -> Result<File> {
        regular(&self.root, path, false)
    }
}
fn create(dir: &File, name: &str) -> Result<File> {
    Ok(File::from(rfs::openat2(
        dir,
        name,
        OFlags::RDWR | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC,
        Mode::from_bits_retain(0o600),
        RESOLVE,
    )?))
}
/// O_PATH inspects type without opening a device or waiting on a FIFO. The proc descriptor
/// reference below is generated internally and reopens the exact checked inode, not user text.
fn regular(dir: &File, path: &str, write: bool) -> Result<File> {
    let pinned = File::from(rfs::openat2(
        dir,
        path,
        OFlags::PATH | OFlags::CLOEXEC,
        Mode::empty(),
        RESOLVE,
    )?);
    let metadata = pinned.metadata()?;
    ensure!(
        metadata.is_file() && metadata.nlink() == 1,
        "file must be regular and singly linked"
    );
    let file = OpenOptions::new()
        .read(true)
        .write(write)
        .custom_flags((OFlags::CLOEXEC | OFlags::NONBLOCK).bits() as i32)
        .open(format!("/proc/self/fd/{}", pinned.as_raw_fd()))?;
    Ok(file)
}
pub(super) fn read_json<T: DeserializeOwned>(dir: &File, name: &str) -> Result<T> {
    let mut bytes = Vec::new();
    regular(dir, name, false)?
        .take(16385)
        .read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= 16384, "file metadata exceeds bound");
    serde_json::from_slice(&bytes).map_err(|_| anyhow::anyhow!("invalid file metadata"))
}
pub(super) fn write_json(dir: &File, name: &str, value: &impl Serialize) -> Result<()> {
    let bytes = serde_json::to_vec(value)?;
    ensure!(bytes.len() <= 16384, "file metadata exceeds bound");
    let temp = format!("{}.tmp", OperationId::generate());
    let mut file = create(dir, &temp)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    rfs::renameat(dir, &temp, dir, name)?;
    dir.sync_all()?;
    Ok(())
}
pub(super) fn version(file: &File) -> Result<(u64, i64, i64, i64, i64)> {
    let meta = file.metadata()?;
    Ok((
        meta.len(),
        meta.mtime(),
        meta.mtime_nsec(),
        meta.ctime(),
        meta.ctime_nsec(),
    ))
}
