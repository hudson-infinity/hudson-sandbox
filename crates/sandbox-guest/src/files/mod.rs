//! Synchronous guest-local file engine. Call from a bounded blocking worker, never a host.
//! The VM is the security boundary; guest root can alter its own files and receipts.
mod fs;
use self::fs::{Root, read_json, write_json};
use anyhow::{Context as _, Result, ensure};
use sandbox_protocol::{OperationId, files::*, guest_model::Context};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    io::{Read, Seek, SeekFrom, Write},
    path::Path,
    sync::{Arc, atomic::Ordering},
};

pub struct Transfers {
    root: Arc<Root>,
    context: Context,
    receipts: BTreeMap<OperationId, Receipt>,
    // Any uncertain local metadata write fences this engine until reopening.
    failed: bool,
}
impl Transfers {
    /// Root is an existing operator-selected workspace on a local writable filesystem.
    /// Retained context and receipts must match; no ambiguous mutation is replayed.
    pub fn open(path: &Path, context: Context) -> Result<Self> {
        ensure!(
            context.generation > 0 && !context.boot_id.is_empty() && context.boot_id.len() <= 64,
            "invalid transfer context"
        );
        let root = Arc::new(Root::open(path)?);
        let names = root.names()?;
        if names.iter().any(|n| n == "context.json") {
            ensure!(
                read_json::<Context>(&root.state, "context.json")? == context,
                "file workspace context mismatch"
            );
        } else {
            ensure!(
                names.iter().all(|n| n == "lock"),
                "unbound file transfer state"
            );
            write_json(&root.state, "context.json", &context)?;
        }
        let mut receipts = BTreeMap::new();
        let mut reserved = 0u64;
        for name in &names {
            if name == "lock" || name == "context.json" {
                continue;
            }
            if let Some(id) = name.strip_suffix(".json") {
                let id: OperationId = id.parse()?;
                let mut receipt: Receipt = read_json(&root.state, name)?;
                receipt.validate()?;
                ensure!(
                    receipt.context == context && receipt.upload.operation_id == id,
                    "file receipt identity mismatch"
                );
                reserved = reserved
                    .checked_add(receipt.upload.size)
                    .context("file capacity overflow")?;
                ensure!(
                    reserved <= MAX_RESERVED_BYTES && receipts.len() < MAX_TRANSFERS,
                    "retained file transfer capacity exceeded"
                );
                if receipt.state == State::CommitIntent {
                    receipt.state = State::Unknown;
                    write_json(&root.state, name, &receipt)?;
                }
                ensure!(
                    receipts.insert(id, receipt).is_none(),
                    "duplicate file receipt"
                );
            } else {
                ensure!(
                    name.ends_with(".data") || name.ends_with(".tmp"),
                    "unexpected transfer state entry"
                );
                let stem = name
                    .strip_suffix(".data")
                    .or_else(|| name.strip_suffix(".tmp"))
                    .context("invalid state filename")?;
                stem.parse::<OperationId>()?;
            }
        }
        // Orphan chunks/temporary metadata were never accepted without a receipt. Remove only
        // validated internal basenames; unlink never follows a substituted symlink.
        for name in &names {
            if let Some(id) = name.strip_suffix(".data") {
                let id = id.parse::<OperationId>()?;
                if receipts.get(&id).is_none_or(|r| r.state != State::Staging) {
                    root.remove(name)?;
                } else {
                    let file = root.stage(id, false)?;
                    ensure!(
                        file.metadata()?.len() <= receipts[&id].upload.size,
                        "oversized staging file"
                    );
                }
            } else if name.ends_with(".tmp") {
                root.remove(name)?;
            }
        }
        for receipt in receipts.values().filter(|r| r.state == State::Staging) {
            // Missing staging after admission is corruption, not permission to reset an upload.
            root.stage(receipt.upload.operation_id, false)?;
        }
        Ok(Self {
            root,
            context,
            receipts,
            failed: false,
        })
    }
    fn healthy(&self) -> Result<()> {
        ensure!(!self.failed, "file engine requires recovery");
        Ok(())
    }
    fn save(&mut self, receipt: Receipt) -> Result<Receipt> {
        if let Err(error) = write_json(
            &self.root.state,
            &format!("{}.json", receipt.upload.operation_id),
            &receipt,
        ) {
            self.failed = true;
            return Err(error);
        }
        self.receipts
            .insert(receipt.upload.operation_id, receipt.clone());
        Ok(receipt)
    }
    pub fn inspect(&self, id: OperationId) -> Result<Option<Receipt>> {
        self.healthy()?;
        Ok(self.receipts.get(&id).cloned())
    }
    pub fn begin(&mut self, upload: Upload) -> Result<Receipt> {
        self.healthy()?;
        upload.validate()?;
        let digest = upload.digest()?;
        if let Some(receipt) = self.receipts.get(&upload.operation_id) {
            ensure!(
                receipt.digest == digest && receipt.upload == upload,
                "file operation conflicts"
            );
            return Ok(receipt.clone());
        }
        let reserved: u64 = self.receipts.values().map(|r| r.upload.size).sum();
        ensure!(
            self.receipts.len() < MAX_TRANSFERS && reserved + upload.size <= MAX_RESERVED_BYTES,
            "file transfer capacity exhausted"
        );
        // Validate the current parent before reserving space. Commit resolves it again.
        self.root.parent(&upload.path)?;
        if let Err(error) = (|| -> Result<()> {
            self.root.stage(upload.operation_id, true)?.sync_all()?;
            self.root.state.sync_all()?;
            Ok(())
        })() {
            self.failed = true;
            return Err(error);
        }
        self.save(Receipt {
            version: 1,
            context: self.context.clone(),
            upload,
            digest,
            state: State::Staging,
        })
    }
    /// Contiguous chunks or byte-identical retries only. A partial write can resume by
    /// verifying its existing prefix and appending the remainder. No staged bytes are replaced.
    pub fn write_chunk(&mut self, id: OperationId, offset: u64, data: &[u8]) -> Result<u64> {
        self.healthy()?;
        ensure!(
            !data.is_empty() && data.len() <= MAX_CHUNK_BYTES,
            "invalid file chunk length"
        );
        let receipt = self.receipts.get(&id).context("file operation not found")?;
        ensure!(
            receipt.state == State::Staging,
            "file upload no longer writable"
        );
        let end = offset
            .checked_add(data.len() as u64)
            .context("file offset overflow")?;
        ensure!(
            end <= receipt.upload.size,
            "chunk exceeds declared file size"
        );
        let mut file = self.root.stage(id, false)?;
        let size = file.metadata()?.len();
        ensure!(
            size <= receipt.upload.size && offset <= size,
            "noncontiguous file chunk"
        );
        let overlap = (size - offset).min(data.len() as u64) as usize;
        let mut previous = vec![0; overlap];
        file.seek(SeekFrom::Start(offset))?;
        file.read_exact(&mut previous)?;
        ensure!(previous == data[..overlap], "file chunk retry conflicts");
        if overlap < data.len() {
            file.seek(SeekFrom::End(0))?;
            file.write_all(&data[overlap..])?;
        }
        file.sync_all()?;
        Ok(size.max(end))
    }
    pub fn commit(&mut self, id: OperationId) -> Result<Receipt> {
        self.healthy()?;
        let mut receipt = self
            .receipts
            .get(&id)
            .context("file operation not found")?
            .clone();
        if receipt.state != State::Staging {
            return Ok(receipt);
        }
        let mut stage = self.root.stage(id, false)?;
        ensure!(
            stage.metadata()?.len() == receipt.upload.size,
            "incomplete file upload"
        );
        let before = fs::version(&stage)?;
        let mut hash = Sha256::new();
        let copied = std::io::copy(&mut (&mut stage).take(MAX_FILE_BYTES + 1), &mut hash)?;
        ensure!(
            copied == receipt.upload.size
                && fs::version(&stage)? == before
                && <[u8; 32]>::from(hash.finalize()) == receipt.upload.sha256,
            "file upload digest mismatch"
        );
        let (parent, name) = self.root.parent(&receipt.upload.path)?;
        // No existing destination inode is opened or truncated. Atomic rename replaces the
        // directory entry itself, including a symlink/hardlink, without following that link.
        rustix::fs::fchmod(
            &stage,
            rustix::fs::Mode::from_bits_retain(receipt.upload.mode),
        )?;
        stage.sync_all()?;
        receipt.state = State::CommitIntent;
        self.save(receipt.clone())?;
        if let Err(error) = self.root.publish(id, &parent, &name) {
            // Even failure after rename/fsync has an uncertain external effect. Never retry it.
            receipt.state = State::Unknown;
            self.save(receipt)?;
            return Err(error);
        }
        receipt.state = State::Committed;
        self.save(receipt)
    }
    pub fn abort(&mut self, id: OperationId) -> Result<Receipt> {
        self.healthy()?;
        let mut receipt = self
            .receipts
            .get(&id)
            .context("file operation not found")?
            .clone();
        if receipt.state == State::Staging {
            receipt.state = State::Aborted;
            self.save(receipt.clone())?;
            self.root.remove(&format!("{id}.data"))?;
        }
        Ok(receipt)
    }
    /// Captures one bounded byte sequence from a pinned regular file. This is not an atomic
    /// filesystem snapshot. A detected concurrent change rejects the capture. The returned
    /// digest covers exactly the captured bytes and subsequent chunks never reread the path.
    pub fn capture(&self, path: &str) -> Result<Download> {
        self.healthy()?;
        validate_path(path)?;
        let mut file = self.root.read(path)?;
        let before = fs::version(&file)?;
        ensure!(before.0 <= MAX_FILE_BYTES, "download exceeds file limit");
        let permit = DownloadPermit::acquire(self.root.clone())?;
        let mut bytes = Vec::with_capacity(before.0 as usize + 1);
        (&mut file).take(before.0 + 1).read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() as u64 == before.0 && fs::version(&file)? == before,
            "file changed during capture"
        );
        let sha256 = Sha256::digest(&bytes).into();
        Ok(Download {
            bytes,
            sha256,
            _permit: permit,
        })
    }
}

/// Holds at most MAX_FILE_BYTES. At most eight captures per open workspace remain live.
/// A capture retains workspace ownership until dropped, including after the engine is dropped.
/// File contents are deliberately absent from Debug and receipts.
pub struct Download {
    bytes: Vec<u8>,
    _permit: DownloadPermit,
    pub sha256: [u8; 32],
}
impl Download {
    pub fn size(&self) -> u64 {
        self.bytes.len() as u64
    }
    pub fn chunk(&self, offset: u64, limit: usize) -> Result<&[u8]> {
        ensure!(
            (1..=MAX_CHUNK_BYTES).contains(&limit) && offset <= self.size(),
            "invalid download range"
        );
        let start = offset as usize;
        Ok(&self.bytes[start..self.bytes.len().min(start + limit)])
    }
}

struct DownloadPermit(Arc<Root>);
impl DownloadPermit {
    fn acquire(root: Arc<Root>) -> Result<Self> {
        root.downloads
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                (current < 8).then_some(current + 1)
            })
            .map_err(|_| anyhow::anyhow!("download capture capacity exhausted"))?;
        Ok(Self(root))
    }
}
impl Drop for DownloadPermit {
    fn drop(&mut self) {
        self.0.downloads.fetch_sub(1, Ordering::AcqRel);
    }
}

impl std::fmt::Debug for Transfers {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Transfers")
            .field("context", &self.context)
            .field("receipt_count", &self.receipts.len())
            .field("failed", &self.failed)
            .finish_non_exhaustive()
    }
}
impl std::fmt::Debug for Download {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Download")
            .field("size", &self.size())
            .finish_non_exhaustive()
    }
}
