//! One-host controller. Operator provisioning and TLS are required in every mode.
use clap::Parser;
use sandbox_controller::{Controller, ControllerConfig, Tick};
use sandbox_protocol::HostId;
use sandbox_store::Store;
use std::{collections::BTreeSet, path::PathBuf, time::Duration};

#[derive(Parser)]
struct Args {
    #[arg(long, env = "DATABASE_URL", hide_env_values = true)]
    database_url: String,
    #[arg(long)]
    endpoint: String,
    #[arg(long)]
    host_id: HostId,
    #[arg(long)]
    host_epoch: i64,
    #[arg(long)]
    ca_cert: PathBuf,
    #[arg(long)]
    client_cert: PathBuf,
    #[arg(long)]
    client_key: PathBuf,
    #[arg(long, required = true)]
    image_digest: Vec<String>,
    /// Development only. Persist and expose simulated evidence; never run against production data.
    #[arg(long, default_value_t = false)]
    allow_simulated: bool,
    /// Run one maintenance/operation tick and exit; useful for controlled diagnostics.
    #[arg(long)]
    once: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let store = Store::connect(&args.database_url, 8).await?;
    store.migrate().await?;
    let mut controller = Controller::connect(
        store,
        ControllerConfig {
            endpoint: args.endpoint,
            host: args.host_id,
            epoch: args.host_epoch,
            allowed_images: BTreeSet::from_iter(args.image_digest),
            allow_simulated: args.allow_simulated,
        },
        &std::fs::read(args.ca_cert)?,
        &std::fs::read(args.client_cert)?,
        &std::fs::read(args.client_key)?,
    )
    .await?;
    if args.once {
        eprintln!("{:?}", controller.tick().await?);
        return Ok(());
    }
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => break,
            result = controller.tick() => match result {
                Ok(Tick::Idle) => {},
                Ok(tick) => eprintln!("sandbox controller: {tick:?}"),
                Err(error) => eprintln!("sandbox controller: {error}"),
            }
        }
        tokio::select! { _ = tokio::signal::ctrl_c() => break, _ = tokio::time::sleep(Duration::from_millis(500)) => {} }
    }
    Ok(())
}
