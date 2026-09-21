//! Guest-only init and allocation bootstrap. Never invoke on a host.
use anyhow::{Result, ensure};
use sandbox_protocol::bootstrap::Bootstrap;
use std::{
    fs,
    os::unix::fs::{FileTypeExt, OpenOptionsExt},
    path::Path,
    process::{Command, Stdio},
};

fn mount(source: &str, target: &str, kind: &str) -> Result<()> {
    fs::create_dir_all(target)?;
    rustix::mount::mount(
        source,
        target,
        kind,
        rustix::mount::MountFlags::NOSUID | rustix::mount::MountFlags::NOEXEC,
        None,
    )?;
    Ok(())
}
/// Our init owns the guest boot. Customer commands remain UID 0 in writable userspace.
pub fn init() -> Result<()> {
    ensure!(
        std::process::id() == 1 && rustix::process::geteuid().is_root(),
        "guest boot requires PID 1"
    );
    mount("proc", "/proc", "proc")?;
    mount("sysfs", "/sys", "sysfs")?;
    mount("devtmpfs", "/dev", "devtmpfs")?;
    mount("cgroup2", "/sys/fs/cgroup", "cgroup2")?;
    for path in [
        "/run/hudson",
        "/root",
        "/tmp",
        "/sys/fs/cgroup/system",
        "/sys/fs/cgroup/workloads",
    ] {
        fs::create_dir_all(path)?;
    }
    fs::write("/sys/fs/cgroup/cgroup.subtree_control", "+pids")?;
    fs::write("/sys/fs/cgroup/workloads/cgroup.subtree_control", "+pids")?;
    let status = Command::new(std::env::current_exe()?)
        .arg("__boot-namespace")
        .env_clear()
        .stdin(Stdio::null())
        .status()?;
    ensure!(status.success(), "guest agent exited unsuccessfully");
    // The host guardian remains authoritative for VM teardown, including agent death.
    anyhow::bail!("guest agent exited")
}
/// A single-threaded helper separates the agent PID namespace before starting its runtime.
pub fn namespace() -> Result<()> {
    ensure!(
        rustix::process::geteuid().is_root(),
        "guest agent requires root"
    );
    fs::write(
        "/sys/fs/cgroup/system/cgroup.procs",
        std::process::id().to_string(),
    )?;
    // SAFETY: called by the binary before creating threads; FILES is not requested.
    #[allow(unsafe_code)]
    unsafe {
        rustix::thread::unshare_unsafe(
            rustix::thread::UnshareFlags::NEWPID | rustix::thread::UnshareFlags::NEWNS,
        )?;
    }
    rustix::mount::mount_change(
        "/",
        rustix::mount::MountPropagationFlags::PRIVATE | rustix::mount::MountPropagationFlags::REC,
    )?;
    let status = Command::new(std::env::current_exe()?)
        .arg("__serve-bootstrap")
        .env_clear()
        .stdin(Stdio::null())
        .status()?;
    ensure!(status.success(), "guest bootstrap agent failed");
    Ok(())
}
pub fn serve() -> Result<()> {
    ensure!(
        std::process::id() == 1 && rustix::process::geteuid().is_root(),
        "guest agent must be namespace init"
    );
    mount("proc", "/proc", "proc")?;
    let device = fs::OpenOptions::new()
        .read(true)
        .custom_flags((rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK).bits() as i32)
        .open("/dev/vdb")?;
    ensure!(
        device.metadata()?.file_type().is_block_device(),
        "bootstrap must be the dedicated block device"
    );
    let bootstrap = Bootstrap::read_device(device)?;
    let tls = bootstrap.server_tls(crate::runner::now_ms())?;
    rustix::process::set_dumpable_behavior(rustix::process::DumpableBehavior::NotDumpable)?;
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(async move {
            let runner = crate::runner::Runner::open(crate::runner::Config {
                state_dir: Path::new("/run/hudson/agent").into(),
                cgroup_root: Path::new("/sys/fs/cgroup/workloads").into(),
                launcher: std::env::current_exe()?,
                context: crate::model::Context {
                    allocation_id: bootstrap.allocation,
                    generation: bootstrap.generation,
                    boot_id: fs::read_to_string("/proc/sys/kernel/random/boot_id")?
                        .trim()
                        .into(),
                },
            })
            .await?;
            use tokio::signal::unix::{SignalKind, signal};
            let mut stop = signal(SignalKind::terminate())?;
            let mut interrupt = signal(SignalKind::interrupt())?;
            let remaining = bootstrap
                .valid_until_unix_ms
                .saturating_sub(crate::runner::now_ms());
            ensure!(remaining > 0, "guest identity expired during boot");
            let served = tokio::select! {
                result=crate::server::serve_vsock(runner.clone(),tls,52)=>result,
                _=stop.recv()=>Ok(()),
                _=interrupt.recv()=>Ok(()),
                _=tokio::time::sleep(std::time::Duration::from_millis(remaining as u64))=>Ok(()),
            };
            runner.shutdown().await?;
            served
        })
}
