//! Guest-local execution receipts. Guest-root tampering is not a host isolation boundary.
use crate::{
    launcher::{Envelope, ExitReport},
    model::*,
};
use anyhow::{Context as _, Result, ensure};
use sandbox_protocol::{Id, OperationId};
use serde::{Serialize, de::DeserializeOwned};
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    sync::{Mutex, watch},
    time::{sleep, timeout},
};

#[derive(Debug, Clone)]
pub struct Config {
    pub state_dir: PathBuf,
    pub cgroup_root: PathBuf,
    pub launcher: PathBuf,
    pub context: Context,
}
#[derive(Debug)]
struct Registry {
    receipts: BTreeMap<OperationId, Receipt>,
    active: Option<(OperationId, watch::Sender<bool>)>,
    failed: bool,
}
#[derive(Debug)]
struct Inner {
    config: Config,
    registry: Mutex<Registry>,
    _lock: File,
}
#[derive(Debug, Clone)]
pub struct Runner(Arc<Inner>);

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|v| i64::try_from(v.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}
fn read_json<T: DeserializeOwned>(path: &Path) -> Result<T> {
    let mut bytes = Vec::new();
    File::open(path)?.take(16385).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= 16384, "receipt metadata is too large");
    serde_json::from_slice(&bytes).map_err(|_| anyhow::anyhow!("invalid receipt metadata"))
}
fn write_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let parent = path.parent().context("missing state directory")?;
    let temp = parent.join(format!("{}.tmp", OperationId::generate()));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temp)?;
    file.write_all(&serde_json::to_vec(value)?)?;
    file.sync_all()?;
    fs::rename(&temp, path)?;
    File::open(parent)?.sync_all()?;
    Ok(())
}
fn receipt_path(config: &Config, id: OperationId) -> PathBuf {
    config.state_dir.join(format!("{id}.json"))
}
fn group(config: &Config, id: OperationId) -> PathBuf {
    config.cgroup_root.join(id.to_string())
}
fn absent(path: &Path) -> Result<bool> {
    match fs::metadata(path) {
        Ok(m) => {
            ensure!(m.is_dir(), "expected cgroup directory");
            Ok(false)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(true),
        Err(e) => Err(e.into()),
    }
}
async fn cleanup(path: &Path) -> Result<()> {
    if absent(path)? {
        return Ok(());
    }
    fs::write(path.join("cgroup.kill"), "1")?;
    timeout(Duration::from_secs(3), async {
        loop {
            let events = fs::read_to_string(path.join("cgroup.events"))?;
            if events.lines().any(|l| l == "populated 0") {
                return Ok::<_, anyhow::Error>(());
            }
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .context("process tree cleanup unconfirmed")??;
    // A cooperating command can create child cgroups; remove only empty descendants of our group.
    fn remove_empty(path: &Path) -> Result<()> {
        for entry in fs::read_dir(path)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                remove_empty(&entry.path())?
            }
        }
        fs::remove_dir(path)?;
        Ok(())
    }
    remove_empty(path)
}
impl Runner {
    /// Must run inside a guest with a dedicated cgroup subtree and trusted state directory.
    /// Reopening never replays a nonterminal receipt; it kills the owned tree and records unknown.
    pub async fn open(mut config: Config) -> Result<Self> {
        ensure!(
            config.context.generation > 0,
            "invalid allocation generation"
        );
        let boot = fs::read_to_string("/proc/sys/kernel/random/boot_id")?;
        ensure!(
            config.context.boot_id == boot.trim(),
            "guest boot identity mismatch"
        );
        config.cgroup_root = fs::canonicalize(&config.cgroup_root)?;
        ensure!(
            config.cgroup_root.starts_with("/sys/fs/cgroup/")
                && config.cgroup_root != Path::new("/sys/fs/cgroup"),
            "a dedicated guest cgroup subtree is required"
        );
        ensure!(
            fs::read_to_string(config.cgroup_root.join("cgroup.procs"))?
                .trim()
                .is_empty(),
            "agent must remain outside workload cgroup"
        );
        ensure!(
            config.launcher.is_absolute() && config.launcher.is_file(),
            "launcher must be an absolute file path"
        );
        fs::create_dir_all(&config.state_dir)?;
        config.state_dir = fs::canonicalize(&config.state_dir)?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(config.state_dir.join("lock"))?;
        rustix::fs::flock(&lock, rustix::fs::FlockOperation::NonBlockingLockExclusive)
            .context("guest state is already owned")?;
        let context_path = config.state_dir.join("context.json");
        if context_path.exists() {
            ensure!(
                read_json::<Context>(&context_path)? == config.context,
                "allocation context differs from saved state"
            );
        } else {
            // A missing context cannot authorize adoption of arbitrary pre-existing records.
            ensure!(
                fs::read_dir(&config.state_dir)?.count() == 1,
                "state directory has no context"
            );
            write_json(&context_path, &config.context)?;
        }
        let mut receipts = BTreeMap::new();
        let mut reserved = 0u64;
        let entries = fs::read_dir(&config.state_dir)?
            .take(MAX_RECORDS * 5 + 3)
            .collect::<std::io::Result<Vec<_>>>()?;
        ensure!(
            entries.len() <= MAX_RECORDS * 5 + 2,
            "too many retained state files"
        );
        for entry in entries {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "tmp") {
                fs::remove_file(path)?;
                continue;
            }
            if path == context_path || path.extension().is_none_or(|e| e != "json") {
                continue;
            }
            let mut receipt: Receipt = read_json(&path)?;
            ensure!(
                receipt.version == 1
                    && receipt.context == config.context
                    && path == receipt_path(&config, receipt.operation_id),
                "invalid receipt ownership"
            );
            ensure!(
                (1..=MAX_OUTPUT).contains(&receipt.output_limit)
                    && receipt.stdout.stored <= receipt.stdout.seen
                    && receipt.stderr.stored <= receipt.stderr.seen
                    && receipt
                        .stdout
                        .stored
                        .checked_add(receipt.stderr.stored)
                        .is_some_and(|n| n <= receipt.output_limit)
                    && receipt.stdout.truncated == (receipt.stdout.stored < receipt.stdout.seen)
                    && receipt.stderr.truncated == (receipt.stderr.stored < receipt.stderr.seen)
                    && if receipt.state == State::Exited {
                        receipt.exit_code.is_some() ^ receipt.signal.is_some()
                    } else {
                        receipt.exit_code.is_none() && receipt.signal.is_none()
                    },
                "invalid retained receipt"
            );
            reserved = reserved
                .checked_add(receipt.output_limit)
                .context("output reservation overflow")?;
            ensure!(
                reserved <= MAX_RESERVED_OUTPUT && receipts.len() < MAX_RECORDS,
                "retention capacity exceeded"
            );
            if !receipt.state.terminal() || !receipt.cleanup_confirmed {
                cleanup(&group(&config, receipt.operation_id)).await?;
                receipt.state = State::Unknown;
                receipt.cleanup_confirmed = true;
                receipt.exit_code = None;
                receipt.signal = None;
                receipt.reason = Some("agent_restart_after_launch_intent".into());
                write_json(&path, &receipt)?;
            }
            ensure!(
                receipts.insert(receipt.operation_id, receipt).is_none(),
                "duplicate receipt"
            );
        }
        // Unknown cgroups cannot be silently adopted, killed or used for new work.
        for entry in fs::read_dir(&config.cgroup_root)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                anyhow::bail!("unreconciled workload cgroup remains")
            }
        }
        Ok(Self(Arc::new(Inner {
            config,
            registry: Mutex::new(Registry {
                receipts,
                active: None,
                failed: false,
            }),
            _lock: lock,
        })))
    }
    pub async fn start(&self, request: Execute) -> Result<Receipt> {
        request.validate()?;
        let digest = request.digest()?;
        let mut registry = self.0.registry.lock().await;
        if let Some(existing) = registry.receipts.get(&request.operation_id) {
            ensure!(existing.digest == digest, "operation payload conflict");
            return Ok(existing.clone());
        }
        ensure!(!registry.failed, "runner requires host recovery");
        ensure!(registry.active.is_none(), "another command is active");
        let remaining = request
            .deadline_unix_ms
            .checked_sub(now_ms())
            .context("invalid deadline")?;
        ensure!(
            remaining > 0 && remaining <= 6 * 60 * 60 * 1000,
            "deadline must be in the next six hours"
        );
        ensure!(
            registry.receipts.len() < MAX_RECORDS,
            "receipt capacity exhausted"
        );
        let reserved: u64 = registry.receipts.values().map(|v| v.output_limit).sum();
        ensure!(
            reserved + request.output_limit <= MAX_RESERVED_OUTPUT,
            "output reservation capacity exhausted"
        );
        let receipt = Receipt {
            version: 1,
            context: self.0.config.context.clone(),
            operation_id: request.operation_id,
            digest,
            state: State::LaunchIntent,
            deadline_unix_ms: request.deadline_unix_ms,
            output_limit: request.output_limit,
            cancel_requested: false,
            cleanup_confirmed: false,
            exit_code: None,
            signal: None,
            stdout: Output::default(),
            stderr: Output::default(),
            reason: None,
        };
        // No process is spawned until this intent and its directory entry have been synced.
        write_json(
            &receipt_path(&self.0.config, request.operation_id),
            &receipt,
        )?;
        let (cancel, signal) = watch::channel(false);
        registry
            .receipts
            .insert(request.operation_id, receipt.clone());
        registry.active = Some((request.operation_id, cancel));
        let runner = self.clone();
        tokio::spawn(async move {
            runner.run(request, signal).await;
        });
        Ok(receipt)
    }
    pub async fn inspect(&self, id: OperationId) -> Option<Receipt> {
        self.0.registry.lock().await.receipts.get(&id).cloned()
    }
    pub async fn cancel(&self, id: OperationId) -> Result<Receipt> {
        let mut registry = self.0.registry.lock().await;
        let mut receipt = registry
            .receipts
            .get(&id)
            .context("unknown operation")?
            .clone();
        if receipt.state.terminal() {
            return Ok(receipt.clone());
        }
        receipt.cancel_requested = true;
        write_json(&receipt_path(&self.0.config, id), &receipt)?;
        registry.receipts.insert(id, receipt.clone());
        if let Some((active, cancel)) = &registry.active {
            ensure!(*active == id, "operation is not owned");
            let _ = cancel.send(true);
        }
        Ok(receipt)
    }
    async fn run(&self, request: Execute, mut cancel: watch::Receiver<bool>) {
        let id = request.operation_id;
        let result = self.execute(&request, &mut cancel).await;
        let cleaned = cleanup(&group(&self.0.config, id)).await.is_ok();
        let mut registry = self.0.registry.lock().await;
        let Some(receipt) = registry.receipts.get_mut(&id) else {
            registry.failed = true;
            return;
        };
        receipt.cleanup_confirmed = cleaned;
        match result {
            Ok((state, report, stdout, stderr)) if cleaned => {
                receipt.state = state;
                receipt.exit_code = report.and_then(|r| r.exit_code);
                receipt.signal = report.and_then(|r| r.signal);
                receipt.stdout = stdout;
                receipt.stderr = stderr;
            }
            _ => {
                receipt.state = State::Unknown;
                receipt.reason = Some("execution_or_cleanup_unconfirmed".into());
            }
        }
        if write_json(&receipt_path(&self.0.config, id), receipt).is_err() {
            receipt.state = State::Unknown;
            receipt.exit_code = None;
            receipt.signal = None;
            receipt.reason = Some("receipt_commit_unconfirmed".into());
            registry.failed = true;
        }
        if !cleaned {
            registry.failed = true;
        }
        registry.active = None;
    }
    async fn execute(
        &self,
        request: &Execute,
        cancel: &mut watch::Receiver<bool>,
    ) -> Result<(State, Option<ExitReport>, Output, Output)> {
        if *cancel.borrow() {
            return Ok((State::Cancelled, None, Output::default(), Output::default()));
        }
        if now_ms() >= request.deadline_unix_ms {
            return Ok((State::TimedOut, None, Output::default(), Output::default()));
        }
        let config = &self.0.config;
        let cgroup = group(config, request.operation_id);
        fs::create_dir(&cgroup)?;
        fs::write(cgroup.join("pids.max"), "256")?;
        let stdout = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(
                config
                    .state_dir
                    .join(format!("{}.stdout", request.operation_id)),
            )?;
        let stderr = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(
                config
                    .state_dir
                    .join(format!("{}.stderr", request.operation_id)),
            )?;
        let report = config
            .state_dir
            .join(format!("{}.exit", request.operation_id));
        let packet = serde_json::to_vec(&Envelope {
            request: request.clone(),
            cgroup: cgroup.clone(),
            report: report.clone(),
        })?;
        let mut child = tokio::process::Command::new(&config.launcher)
            .arg("__launch")
            .env_clear()
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;
        let mut input = child.stdin.take().context("launcher input missing")?;
        let budget = Arc::new(AtomicU64::new(request.output_limit));
        let out = child.stdout.take().context("launcher stdout missing")?;
        let err = child.stderr.take().context("launcher stderr missing")?;
        let (failed, mut capture_failure) = tokio::sync::mpsc::channel(3);
        let spawn_capture = |reader, file, budget, failed: tokio::sync::mpsc::Sender<()>| {
            tokio::spawn(async move {
                let result = capture(reader, file, budget).await;
                if result.is_err() {
                    let _ = failed.send(()).await;
                }
                result
            })
        };
        // Erase only the pipe types so both streams share one bounded capture implementation.
        let mut out_task = spawn_capture(
            Box::new(out) as Box<dyn AsyncRead + Unpin + Send>,
            stdout,
            budget.clone(),
            failed.clone(),
        );
        let mut err_task = spawn_capture(
            Box::new(err) as Box<dyn AsyncRead + Unpin + Send>,
            stderr,
            budget,
            failed.clone(),
        );
        let input_task = tokio::spawn(async move {
            let result = input.write_all(&packet).await;
            drop(input);
            if result.is_err() {
                let _ = failed.send(()).await;
            }
        });
        let duration = Duration::from_millis(u64::try_from(
            request.deadline_unix_ms.saturating_sub(now_ms()).max(0),
        )?);
        let deadline = sleep(duration);
        tokio::pin!(deadline);
        let mut wall_check = tokio::time::interval(Duration::from_millis(100));
        let (state, waited) = loop {
            tokio::select! {
                biased;
                changed=cancel.changed()=>{if changed.is_ok() && *cancel.borrow(){break (State::Cancelled,false)}},
                _=&mut deadline=>break (State::TimedOut,false),
                _=wall_check.tick()=>{if now_ms()>=request.deadline_unix_ms {break (State::TimedOut,false)}},
                Some(())=capture_failure.recv()=>break (State::Unknown,false),
                status=child.wait()=>{break (if status.is_ok_and(|s|s.success()){State::Exited}else{State::Unknown},true)},
            }
        };
        // cgroup.kill handles forks/setsid; a PID or process-group exit alone is insufficient.
        let cleaned = cleanup(&cgroup).await;
        if !waited {
            let _ = child.kill().await;
            let _ = child.wait().await;
        }
        let captures = timeout(Duration::from_secs(2), async {
            let out = (&mut out_task).await??;
            let err = (&mut err_task).await??;
            Ok::<_, anyhow::Error>((out, err))
        })
        .await;
        out_task.abort();
        err_task.abort();
        input_task.abort();
        let (stdout, stderr) = captures.context("output completion unconfirmed")??;
        cleaned?;
        let report = if state == State::Exited {
            let result: ExitReport = read_json(&report)?;
            ensure!(
                result.version == 1 && (result.exit_code.is_some() ^ result.signal.is_some()),
                "invalid command exit report"
            );
            Some(result)
        } else {
            None
        };
        Ok((state, report, stdout, stderr))
    }
}
async fn capture(
    mut input: Box<dyn AsyncRead + Unpin + Send>,
    file: File,
    budget: Arc<AtomicU64>,
) -> Result<Output> {
    let mut output = tokio::fs::File::from_std(file);
    let mut result = Output::default();
    let mut bytes = [0u8; 8192];
    loop {
        let count = input.read(&mut bytes).await?;
        if count == 0 {
            break;
        }
        result.seen = result.seen.saturating_add(count as u64);
        let before = budget
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |left| {
                Some(left.saturating_sub(count as u64))
            })
            .unwrap_or(0);
        let keep = count.min(usize::try_from(before).unwrap_or(usize::MAX));
        output.write_all(&bytes[..keep]).await?;
        result.stored += keep as u64;
        result.truncated |= keep < count;
    }
    output.sync_all().await?;
    Ok(result)
}
