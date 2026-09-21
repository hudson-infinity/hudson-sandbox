use super::*;
use crate::lease::Deadline;
use std::{
    os::unix::net::{UnixListener, UnixStream},
    process::{Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

// An ancestor namespace's client has PID 0 when observed from namespace init.
// Read libc::ucred: rustix::UCred requires a nonzero Pid even though only UID
// determines authorization here. We do not enter a user namespace.
fn root_peer(socket: &UnixStream) -> Result<bool> {
    use std::os::fd::AsRawFd;
    let mut cred = libc::ucred {
        pid: 0,
        uid: u32::MAX,
        gid: u32::MAX,
    };
    let mut length = std::mem::size_of_val(&cred) as libc::socklen_t;
    // SAFETY: live socket FD, correctly sized and aligned initialized output,
    // and a valid length pointer. No pointers escape this synchronous call.
    #[allow(unsafe_code)]
    let result = unsafe {
        libc::getsockopt(
            socket.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut cred as *mut libc::ucred).cast(),
            &mut length,
        )
    };
    ensure!(
        result == 0 && length as usize == std::mem::size_of_val(&cred),
        "guardian peer credentials unavailable"
    );
    Ok(cred.uid == 0)
}
const CONTROL_LIMIT: usize = 8;
const CONTROL_BYTES: usize = 65536;
fn read_frame<T: DeserializeOwned>(io: &mut UnixStream, budget: Duration) -> Result<T> {
    let until = std::time::Instant::now() + budget;
    fn exact(io: &mut UnixStream, mut bytes: &mut [u8], until: std::time::Instant) -> Result<()> {
        while !bytes.is_empty() {
            let remaining = until
                .checked_duration_since(std::time::Instant::now())
                .context("guardian read deadline")?;
            io.set_read_timeout(Some(remaining))?;
            let n = io.read(bytes)?;
            ensure!(n > 0, "guardian connection closed");
            bytes = &mut bytes[n..];
        }
        Ok(())
    }
    let mut header = [0; 4];
    exact(io, &mut header, until)?;
    let len = u32::from_be_bytes(header) as usize;
    ensure!(len > 0 && len <= CONTROL_BYTES, "invalid guardian frame");
    let mut bytes = vec![0; len];
    exact(io, &mut bytes, until)?;
    serde_json::from_slice(&bytes).map_err(|_| anyhow::anyhow!("invalid guardian request"))
}
fn write_frame(io: &mut UnixStream, value: &impl Serialize, budget: Duration) -> Result<()> {
    let bytes = serde_json::to_vec(value)?;
    ensure!(
        !bytes.is_empty() && bytes.len() <= CONTROL_BYTES,
        "invalid guardian frame"
    );
    let mut frame = (bytes.len() as u32).to_be_bytes().to_vec();
    frame.extend_from_slice(&bytes);
    let mut rest = frame.as_slice();
    let until = std::time::Instant::now() + budget;
    while !rest.is_empty() {
        let remaining = until
            .checked_duration_since(std::time::Instant::now())
            .context("guardian write deadline")?;
        io.set_write_timeout(Some(remaining))?;
        let written = io.write(rest)?;
        ensure!(written > 0, "guardian connection closed");
        rest = &rest[written..];
    }
    Ok(())
}
/// Blocking wrapper outside the guardian PID namespace. Losing this process cannot disable expiry.
pub fn launch(manifest: Manifest) -> Result<Receipt> {
    manifest.validate()?;
    let existing = match manifest.prepare() {
        Ok(record) => record,
        Err(error) if error.is::<OwnershipBusy>() => {
            let response = control(&manifest, Action::Inspect)?;
            ensure!(
                response.error.is_none(),
                "active guardian inspection failed"
            );
            return response.receipt.context("active guardian receipt missing");
        }
        Err(error) => return Err(error),
    };
    if existing.state == State::Stopped {
        return Ok(existing);
    }
    if existing.state != State::Prepared {
        return manifest.reconcile("guardian_recovery_fence");
    }
    ensure!(
        existing.host_boot_id == boot_id()?,
        "prepared allocation belongs to another host boot"
    );
    // SAFETY: no descriptor-table separation is requested. The CLI enters here before threads.
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
        .arg("__guardian-init")
        .arg(manifest.directory().join("manifest.json"))
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    let reason = if status.code() == Some(124) {
        "lease_expired"
    } else if status.success() {
        "guardian_stopped"
    } else {
        "guardian_lost"
    };
    manifest.reconcile(reason)
}
fn process_identity() -> Result<(u32, u64)> {
    let stat = fs::read_to_string("/proc/self/stat")?;
    let pid = stat
        .split_whitespace()
        .next()
        .context("missing guardian PID")?
        .parse()?;
    let fields = stat.rsplit_once(") ").context("invalid guardian stat")?.1;
    let ticks = fields
        .split_whitespace()
        .nth(19)
        .context("missing guardian start time")?
        .parse()?;
    Ok((pid, ticks))
}
/// Must be PID 1. Exiting this process causes Linux to kill every namespace descendant.
pub fn namespace_init(manifest: Manifest) -> Result<()> {
    root()?;
    ensure!(std::process::id() == 1, "guardian must be namespace init");
    manifest.validate()?;
    // Keep the flock until the kernel closes descriptors on PID-namespace init death.
    // Releasing it while unwinding an error could let recovery race still-live descendants.
    let _lock = std::mem::ManuallyDrop::new(manifest.lock()?);
    let mut receipt = manifest.receipt()?;
    ensure!(
        receipt.state == State::Prepared && receipt.host_boot_id == boot_id()?,
        "allocation is not eligible for launch"
    );
    manifest.verify_bootstrap(&receipt)?;
    let (host_pid, start_ticks) = process_identity()?;
    rustix::mount::mount(
        "proc",
        "/proc",
        "proc",
        rustix::mount::MountFlags::NOSUID
            | rustix::mount::MountFlags::NODEV
            | rustix::mount::MountFlags::NOEXEC,
        None,
    )?;
    let deadline = Arc::new(Deadline::new(
        receipt.expires_unix_ms,
        wall_ms(),
        boot_ms(),
    )?);
    let watchdog = deadline.clone();
    // No journal or request mutex is taken here. PID-namespace teardown is the kill mechanism.
    std::thread::Builder::new()
        .name("allocation-deadline".into())
        .spawn(move || {
            loop {
                if watchdog.expire(boot_ms()) {
                    std::process::exit(124);
                }
                if watchdog.value() == 0 {
                    std::process::exit(0);
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        })?;
    let parent = &manifest.config.cgroup_parent;
    match fs::create_dir(parent) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e.into()),
    }
    ensure!(
        fs::canonicalize(parent)? == *parent
            && fs::read_to_string(parent.join("cgroup.procs"))?
                .trim()
                .is_empty(),
        "invalid delegated guardian cgroup parent"
    );
    fs::write(parent.join("cgroup.subtree_control"), "+cpu +memory +pids")?;
    fs::create_dir(manifest.group())?;
    receipt.state = State::LaunchIntent;
    receipt.cgroup_inode = Some(fs::metadata(manifest.group())?.ino());
    receipt.guardian_host_pid = Some(host_pid);
    receipt.guardian_start_ticks = Some(start_ticks);
    write_json(&manifest.record_path(), &receipt)?;
    // The journal pins the cgroup inode before any VM process can join it.
    let listener = UnixListener::bind(manifest.socket())?;
    fs::set_permissions(manifest.socket(), fs::Permissions::from_mode(0o600))?;
    listener.set_nonblocking(true)?;
    let run = manifest.directory().join("run");
    let parent_name = parent
        .strip_prefix("/sys/fs/cgroup")?
        .to_str()
        .context("invalid cgroup name")?;
    let mut jailer = Command::new(run.join("jailer"))
        .args([
            "--id",
            &manifest.start.owner.allocation.uuid().to_string(),
            "--exec-file",
        ])
        .arg(run.join("firecracker"))
        .args([
            "--uid",
            &manifest.config.jail_uid.to_string(),
            "--gid",
            &manifest.config.jail_gid.to_string(),
            "--chroot-base-dir",
        ])
        .arg(run.join("chroots"))
        .args([
            "--new-pid-ns",
            "--cgroup-version",
            "2",
            "--parent-cgroup",
            parent_name,
        ])
        .args([
            "--cgroup",
            &format!("cpu.max={} 100000", manifest.start.vcpu as u64 * 100_000),
            "--cgroup",
            &format!("memory.max={}", (manifest.start.memory_mib + 128) << 20),
            "--cgroup",
            "pids.max=64",
            "--",
            "--no-api",
            "--config-file",
            "/config.json",
        ])
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    let pidfile = manifest.jail_root().join("firecracker.pid");
    let launch_until = std::time::Instant::now() + Duration::from_secs(5);
    let vm_pid = loop {
        if let Ok(value) = fs::read_to_string(&pidfile) {
            let pid: u32 = value.trim().parse()?;
            ensure!(pid > 1, "invalid Firecracker child PID");
            break pid;
        }
        if let Some(status) = jailer.try_wait()? {
            ensure!(status.success(), "jailer launch failed");
        }
        ensure!(
            std::time::Instant::now() < launch_until,
            "Firecracker child identity unconfirmed"
        );
        std::thread::sleep(Duration::from_millis(5));
    };
    ensure!(
        Path::new(&format!("/proc/{vm_pid}")).exists(),
        "Firecracker child already gone"
    );
    receipt.firecracker_namespace_pid = Some(vm_pid);
    receipt.state = State::Running;
    write_json(&manifest.record_path(), &receipt)?;
    let shared = Arc::new(Mutex::new(receipt));
    let slots = Arc::new(AtomicUsize::new(0));
    loop {
        // Reap all adopted children; the jailer's exit is not VM exit.
        loop {
            match rustix::process::wait(rustix::process::WaitOptions::NOHANG) {
                Ok(Some((pid, _))) if pid.as_raw_nonzero().get() as u32 == vm_pid => {
                    let mut record = shared
                        .lock()
                        .map_err(|_| anyhow::anyhow!("guardian registry poisoned"))?;
                    record.state = State::Stopping;
                    record.reason = Some("firecracker_exited".into());
                    write_json(&manifest.record_path(), &*record)?;
                    return Ok(());
                }
                Ok(Some(_)) => {}
                Ok(None) => break,
                Err(rustix::io::Errno::CHILD) => {
                    return Err(anyhow::anyhow!("Firecracker child ownership lost"));
                }
                Err(e) => return Err(anyhow::Error::from(e).context("guardian child wait")),
            }
        }
        match listener.accept() {
            Ok((mut socket, _)) => {
                if !root_peer(&socket)? {
                    continue;
                }
                if slots
                    .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
                        (n < CONTROL_LIMIT).then_some(n + 1)
                    })
                    .is_err()
                {
                    continue;
                }
                let shared = shared.clone();
                let deadline = deadline.clone();
                let manifest = manifest.clone();
                let slots = slots.clone();
                std::thread::spawn(move || {
                    let _ = socket.set_read_timeout(Some(Duration::from_millis(500)));
                    let _ = socket.set_write_timeout(Some(Duration::from_millis(500)));
                    let response = handle(&manifest, &shared, &deadline, &mut socket)
                        .unwrap_or_else(|_| Response {
                            receipt: None,
                            error: Some("request rejected or uncertain".into()),
                        });
                    let _ = write_frame(&mut socket, &response, Duration::from_millis(500));
                    slots.fetch_sub(1, Ordering::SeqCst);
                });
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(10))
            }
            Err(e) => return Err(e.into()),
        }
    }
}
fn handle(
    manifest: &Manifest,
    shared: &Mutex<Receipt>,
    deadline: &Deadline,
    socket: &mut UnixStream,
) -> Result<Response> {
    let request: Request = read_frame(socket, Duration::from_millis(500))?;
    ensure!(
        request.owner == manifest.start.owner,
        "wrong guardian ownership"
    );
    if matches!(request.action, Action::BindGuest) {
        return bind_guest(manifest, shared, deadline);
    }
    let mut record = shared
        .lock()
        .map_err(|_| anyhow::anyhow!("guardian registry poisoned"))?;
    match request.action {
        Action::BindGuest => unreachable!("bind is handled before the mutation lock"),
        Action::Inspect => {}
        Action::Stop => {
            record.state = State::Stopping;
            record.reason = Some("stop_requested".into());
            let saved = write_json(&manifest.record_path(), &*record);
            deadline.stop();
            saved?;
        }
        Action::Renew {
            revision,
            expires_unix_ms,
        } => {
            ensure!(
                record
                    .identity_expires_unix_ms
                    .is_some_and(|until| expires_unix_ms < until),
                "renewal exceeds channel validity"
            );
            let prior = deadline.value();
            ensure!(
                prior > boot_ms() && record.state == State::Running,
                "allocation lease is no longer live"
            );
            ensure!(
                revision > 0 && revision >= record.renewal_revision,
                "stale lease revision"
            );
            if revision == record.renewal_revision {
                ensure!(
                    expires_unix_ms == record.expires_unix_ms,
                    "conflicting renewal retry"
                );
            } else {
                ensure!(
                    expires_unix_ms >= record.expires_unix_ms,
                    "renewal cannot shorten lease"
                );
                Deadline::new(expires_unix_ms, wall_ms(), boot_ms())?;
                let mut renewed = record.clone();
                renewed.renewal_revision = revision;
                renewed.expires_unix_ms = expires_unix_ms;
                if write_json(&manifest.record_path(), &renewed).is_err() {
                    deadline.stop();
                    anyhow::bail!("renewal persistence unconfirmed")
                }
                deadline.extend(prior, expires_unix_ms, wall_ms() as u64, boot_ms())?;
                *record = renewed;
            }
        }
    }
    Ok(Response {
        receipt: Some(record.clone()),
        error: None,
    })
}
fn bind_guest(
    manifest: &Manifest,
    shared: &Mutex<Receipt>,
    deadline: &Deadline,
) -> Result<Response> {
    let before = shared
        .lock()
        .map_err(|_| anyhow::anyhow!("guardian registry poisoned"))?
        .clone();
    ensure!(
        before.state == State::Running && deadline.value() > boot_ms(),
        "allocation is not live"
    );
    let identity = manifest.identity(&before)?;
    let client = crate::guest::GuestClient::from_firecracker_directory(
        manifest.jail_root(),
        52,
        identity.client_tls(wall_ms())?,
        sandbox_protocol::guest_model::Context {
            allocation_id: manifest.start.owner.allocation,
            generation: manifest.start.owner.generation,
            boot_id: before.guest_boot_id.unwrap_or_default(),
        },
    )?;
    let context = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(client.hello())?;
    let mut record = shared
        .lock()
        .map_err(|_| anyhow::anyhow!("guardian registry poisoned"))?;
    ensure!(
        record.state == State::Running
            && deadline.value() > boot_ms()
            && wall_ms() < identity.guest.valid_until_unix_ms,
        "allocation stopped during guest handshake"
    );
    ensure!(
        record
            .guest_boot_id
            .as_ref()
            .is_none_or(|id| *id == context.boot_id),
        "guest boot changed after binding"
    );
    let mut bound = record.clone();
    bound.guest_boot_id = Some(context.boot_id);
    if write_json(&manifest.record_path(), &bound).is_err() {
        deadline.stop();
        anyhow::bail!("guest binding persistence unconfirmed");
    }
    *record = bound;
    Ok(Response {
        receipt: Some(record.clone()),
        error: None,
    })
}
/// Root-only local control. Transport failure never supplies release or renewal evidence.
pub fn control(manifest: &Manifest, action: Action) -> Result<Response> {
    root()?;
    manifest.validate()?;
    let fd = rustix::net::socket_with(
        rustix::net::AddressFamily::UNIX,
        rustix::net::SocketType::STREAM,
        rustix::net::SocketFlags::CLOEXEC | rustix::net::SocketFlags::NONBLOCK,
        None,
    )?;
    // A saturated listener backlog fails promptly instead of blocking connect.
    // Connection uncertainty never authorizes a duplicate launch or renewal.
    rustix::net::connect(&fd, &rustix::net::SocketAddrUnix::new(manifest.socket())?)?;
    rustix::fs::fcntl_setfl(&fd, rustix::fs::OFlags::empty())?;
    let mut socket = UnixStream::from(fd);
    socket.set_read_timeout(Some(Duration::from_secs(2)))?;
    socket.set_write_timeout(Some(Duration::from_secs(2)))?;
    ensure!(root_peer(&socket)?, "guardian peer must be root");
    write_frame(
        &mut socket,
        &Request {
            owner: manifest.start.owner.clone(),
            action,
        },
        Duration::from_secs(2),
    )?;
    let response: Response = read_frame(&mut socket, Duration::from_secs(2))?;
    if let Some(ref receipt) = response.receipt {
        ensure!(
            receipt.owner == manifest.start.owner
                && receipt.digest == manifest.digest()?
                && receipt.host_boot_id == boot_id()?,
            "guardian response ownership mismatch"
        );
    }
    Ok(response)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    #[test]
    fn control_frames_reject_oversize_truncation_and_stalled_bodies() {
        for length in [0u32, CONTROL_BYTES as u32 + 1] {
            let (mut reader, mut writer) = UnixStream::pair().unwrap();
            writer.write_all(&length.to_be_bytes()).unwrap();
            assert!(read_frame::<Request>(&mut reader, Duration::from_millis(100)).is_err());
        }
        let (mut reader, mut writer) = UnixStream::pair().unwrap();
        writer.write_all(&20u32.to_be_bytes()).unwrap();
        writer.write_all(b"{").unwrap();
        drop(writer);
        assert!(read_frame::<Request>(&mut reader, Duration::from_millis(100)).is_err());
        let (mut reader, mut writer) = UnixStream::pair().unwrap();
        writer.write_all(&20u32.to_be_bytes()).unwrap();
        let started = std::time::Instant::now();
        assert!(read_frame::<Request>(&mut reader, Duration::from_millis(100)).is_err());
        assert!(started.elapsed() < Duration::from_secs(1));
    }
    #[test]
    fn control_frame_budget_is_not_reset_by_slow_bytes() {
        let (mut reader, mut writer) = UnixStream::pair().unwrap();
        let sender = std::thread::spawn(move || {
            for byte in [0u8, 0, 0, 64, b'{'] {
                if writer.write_all(&[byte]).is_err() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(70));
            }
        });
        let started = std::time::Instant::now();
        assert!(read_frame::<Request>(&mut reader, Duration::from_millis(160)).is_err());
        assert!(started.elapsed() < Duration::from_millis(300));
        drop(reader);
        sender.join().unwrap();
    }
}
