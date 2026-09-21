//! Diagnostic runner entry point. The supervisor/guest wire server is not implemented here.
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
    if cli().is_err() {
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
