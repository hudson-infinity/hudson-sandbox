#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("the allocation guardian requires Linux");
    std::process::exit(1);
}
#[cfg(target_os = "linux")]
fn main() {
    if run().is_err() {
        eprintln!("allocation guardian failed; reconcile retained ownership before retry");
        std::process::exit(1);
    }
}
#[cfg(target_os = "linux")]
fn run() -> anyhow::Result<()> {
    use sandbox_supervisor::guardian::{self, Action, Manifest};
    use std::path::PathBuf;
    // Namespace setup must occur before any runtime or worker threads exist.
    if std::env::args().nth(1).as_deref() == Some("__guardian-init") {
        let path = std::env::args()
            .nth(2)
            .ok_or_else(|| anyhow::anyhow!("manifest required"))?;
        return guardian::namespace_init(guardian::read_json(std::path::Path::new(&path))?);
    }
    use clap::{Parser, Subcommand};
    #[derive(Parser)]
    struct Args {
        #[arg(long)]
        manifest: PathBuf,
        #[command(subcommand)]
        action: Command,
    }
    #[derive(Subcommand)]
    enum Command {
        Prepare,
        Run,
        Inspect,
        BindGuest,
        Renew {
            #[arg(long)]
            revision: u64,
            #[arg(long)]
            expires_unix_ms: i64,
        },
        Stop,
        Reconcile,
    }
    let args = Args::parse();
    let manifest: Manifest = guardian::read_json(&args.manifest)?;
    let result = match args.action {
        Command::Prepare => serde_json::to_value(manifest.prepare()?)?,
        Command::Run => serde_json::to_value(guardian::launch(manifest)?)?,
        Command::Reconcile => serde_json::to_value(manifest.reconcile("guardian_recovery_fence")?)?,
        Command::BindGuest => {
            serde_json::to_value(guardian::control(&manifest, Action::BindGuest)?)?
        }
        Command::Inspect => serde_json::to_value(guardian::control(&manifest, Action::Inspect)?)?,
        Command::Stop => serde_json::to_value(guardian::control(&manifest, Action::Stop)?)?,
        Command::Renew {
            revision,
            expires_unix_ms,
        } => serde_json::to_value(guardian::control(
            &manifest,
            Action::Renew {
                revision,
                expires_unix_ms,
            },
        )?)?,
    };
    println!("{}", serde_json::to_string(&result)?);
    Ok(())
}
