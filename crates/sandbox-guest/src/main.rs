//! Guest init, authenticated bootstrap listener, and diagnostic runner entry points.
#[cfg(target_os = "linux")]
fn main() {
    let mode = std::env::args().nth(1);
    if matches!(mode.as_deref(), Some("__launch" | "__namespace")) {
        if sandbox_guest::launcher::run(mode.as_deref() == Some("__namespace")).is_err() {
            eprintln!("guest launcher failed");
            std::process::exit(1);
        }
        return;
    }
    let result = match mode.as_deref() {
        Some("__boot-namespace") => sandbox_guest::boot::namespace(),
        Some("__serve-bootstrap") => sandbox_guest::boot::serve(),
        Some("boot") => sandbox_guest::boot::init(),
        None if std::process::id() == 1 => sandbox_guest::boot::init(),
        Some("serve") => serve_cli(),
        _ => cli(),
    };
    if result.is_err() {
        eprintln!("guest runner failed; retained state requires inspection");
        std::process::exit(1);
    }
}
#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("sandbox-guest requires Linux inside a guest VM");
    std::process::exit(1);
}

#[cfg(target_os = "linux")]
fn cli() -> anyhow::Result<()> {
    use clap::Parser;
    use sandbox_guest::{
        model::{Context, Execute},
        runner::{Config, Runner},
    };
    use std::{io::Read, path::PathBuf};
    #[derive(Parser)]
    #[command(
        about = "Guest-only diagnostic command runner; reads one bounded JSON request from stdin"
    )]
    struct Args {
        #[arg(long)]
        state_dir: PathBuf,
        #[arg(long)]
        cgroup_root: PathBuf,
        #[arg(long)]
        allocation_id: sandbox_protocol::AllocationId,
        #[arg(long)]
        generation: i64,
    }
    let args = Args::parse();
    let mut bytes = Vec::new();
    std::io::stdin().take(65537).read_to_end(&mut bytes)?;
    anyhow::ensure!(bytes.len() <= 65536, "request too large");
    let request: Execute =
        serde_json::from_slice(&bytes).map_err(|_| anyhow::anyhow!("invalid execution request"))?;
    let id = request.operation_id;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async move {
        use tokio::signal::unix::{SignalKind, signal};
        let mut interrupt = signal(SignalKind::interrupt())?;
        let mut terminate = signal(SignalKind::terminate())?;
        let runner = Runner::open(Config {
            state_dir: args.state_dir,
            cgroup_root: args.cgroup_root,
            launcher: std::env::current_exe()?,
            context: Context {
                allocation_id: args.allocation_id,
                generation: args.generation,
                boot_id: std::fs::read_to_string("/proc/sys/kernel/random/boot_id")?
                    .trim()
                    .into(),
            },
        })
        .await?;
        runner.start(request).await?;
        loop {
            let receipt = runner
                .inspect(id)
                .await
                .ok_or_else(|| anyhow::anyhow!("missing execution receipt"))?;
            if receipt.state.terminal() {
                println!("{}", serde_json::to_string(&receipt)?);
                anyhow::ensure!(receipt.cleanup_confirmed, "cleanup unconfirmed");
                break;
            }
            tokio::select! {
                _=interrupt.recv()=>{runner.cancel(id).await?;},
                _=terminate.recv()=>{runner.cancel(id).await?;},
                _=tokio::time::sleep(std::time::Duration::from_millis(20))=>{},
            }
        }
        Ok::<_, anyhow::Error>(())
    })
}

#[cfg(target_os = "linux")]
fn serve_cli() -> anyhow::Result<()> {
    use clap::Parser;
    use sandbox_guest::{
        model::Context,
        runner::{Config, Runner},
    };
    use sandbox_protocol::guest_wire::ServerTls;
    use std::{
        io::Read,
        path::{Path, PathBuf},
    };
    #[derive(Parser)]
    struct Args {
        #[arg(long)]
        state_dir: PathBuf,
        #[arg(long)]
        cgroup_root: PathBuf,
        #[arg(long)]
        allocation_id: sandbox_protocol::AllocationId,
        #[arg(long)]
        generation: i64,
        #[arg(long, default_value_t = 52)]
        port: u32,
        #[arg(long)]
        ca: PathBuf,
        #[arg(long)]
        cert: PathBuf,
        #[arg(long)]
        key: PathBuf,
        #[arg(long)]
        host_pin: String,
    }
    fn bounded(path: &Path) -> anyhow::Result<Vec<u8>> {
        let mut data = Vec::new();
        std::fs::File::open(path)?
            .take(65537)
            .read_to_end(&mut data)?;
        anyhow::ensure!(data.len() <= 65536, "TLS file too large");
        Ok(data)
    }
    let args = Args::parse_from(std::env::args().skip(1));
    let pin: [u8; 32] = hex::decode(&args.host_pin)?
        .try_into()
        .map_err(|_| anyhow::anyhow!("invalid host pin"))?;
    let tls = ServerTls::new(
        &bounded(&args.ca)?,
        &bounded(&args.cert)?,
        &bounded(&args.key)?,
        pin,
    )?;
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(async move {
            let runner = Runner::open(Config {
                state_dir: args.state_dir,
                cgroup_root: args.cgroup_root,
                launcher: std::env::current_exe()?,
                context: Context {
                    allocation_id: args.allocation_id,
                    generation: args.generation,
                    boot_id: std::fs::read_to_string("/proc/sys/kernel/random/boot_id")?
                        .trim()
                        .into(),
                },
            })
            .await?;
            use tokio::signal::unix::{SignalKind, signal};
            let mut interrupt = signal(SignalKind::interrupt())?;
            let mut terminate = signal(SignalKind::terminate())?;
            let served = tokio::select! {
                result=sandbox_guest::server::serve_vsock(runner.clone(),tls,args.port)=>result,
                _=interrupt.recv()=>Ok(()),
                _=terminate.recv()=>Ok(()),
            };
            runner.shutdown().await?;
            served
        })
}
