//! Single-threaded launcher stages. Never execute these stages in the host supervisor.
use crate::model::Execute;
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, OpenOptions},
    io::{Read, Write},
    os::unix::{fs::OpenOptionsExt, process::ExitStatusExt},
    path::PathBuf,
    process::{Command, Stdio},
};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Envelope {
    pub request: Execute,
    pub cgroup: PathBuf,
    pub report: PathBuf,
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ExitReport {
    pub version: u32,
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
}

/// The binary calls this before creating a Tokio runtime: unshare must be single-threaded.
pub fn run(namespace_init: bool) -> Result<()> {
    let mut bytes = Vec::new();
    std::io::stdin()
        .take(128 * 1024 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= 128 * 1024, "launcher input too large");
    let envelope: Envelope =
        serde_json::from_slice(&bytes).map_err(|_| anyhow::anyhow!("invalid launcher input"))?;
    envelope.request.validate()?;
    if !namespace_init {
        // Join before unshare/fork, so even failed startup and double-forking descendants are tracked.
        fs::write(
            envelope.cgroup.join("cgroup.procs"),
            std::process::id().to_string(),
        )?;
        // SAFETY: FILES is not requested, so descriptor tables remain shared as required
        // by rustix. This launcher runs before the binary creates any runtime threads.
        #[allow(unsafe_code)]
        unsafe {
            rustix::thread::unshare_unsafe(
                rustix::thread::UnshareFlags::NEWNS | rustix::thread::UnshareFlags::NEWPID,
            )?;
        }
        rustix::mount::mount_change(
            "/",
            rustix::mount::MountPropagationFlags::PRIVATE
                | rustix::mount::MountPropagationFlags::REC,
        )?;
        let mut child = Command::new(std::env::current_exe()?)
            .arg("__namespace")
            .env_clear()
            .stdin(Stdio::piped())
            .spawn()?;
        let sent = child
            .stdin
            .take()
            .ok_or_else(|| anyhow::anyhow!("missing namespace input"))?
            .write_all(&bytes);
        if sent.is_err() {
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!("namespace input failed")
        }
        ensure!(child.wait()?.success(), "namespace execution failed");
        return Ok(());
    }
    ensure!(std::process::id() == 1, "namespace launcher must be PID 1");
    rustix::mount::mount(
        "proc",
        "/proc",
        "proc",
        rustix::mount::MountFlags::NOSUID
            | rustix::mount::MountFlags::NODEV
            | rustix::mount::MountFlags::NOEXEC,
        None,
    )?;
    rustix::process::set_dumpable_behavior(rustix::process::DumpableBehavior::NotDumpable)?;
    let mut report = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&envelope.report)?;
    let mut command = Command::new(&envelope.request.argv[0]);
    command
        .args(&envelope.request.argv[1..])
        .env_clear()
        .env(
            "PATH",
            "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
        )
        .env("HOME", "/root")
        .envs(&envelope.request.env)
        .current_dir(&envelope.request.cwd)
        .stdin(Stdio::null());
    ensure!(
        crate::runner::now_ms() < envelope.request.deadline_unix_ms,
        "command deadline elapsed before spawn"
    );
    let status = command.spawn()?.wait()?;
    let result = ExitReport {
        version: 1,
        exit_code: status.code(),
        signal: status.signal(),
    };
    report.write_all(&serde_json::to_vec(&result)?)?;
    report.sync_all()?;
    // Returning exits this namespace's PID 1, which kills remaining namespace descendants.
    Ok(())
}
