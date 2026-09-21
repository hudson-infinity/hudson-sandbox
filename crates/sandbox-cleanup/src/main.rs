//! Opt-in cleanup service. Credentials and deployment are operator-owned.
use clap::Parser;
use sandbox_artifacts::S3Config;
use sandbox_cleanup::{Cleaner, CleanupTick};
use sandbox_store::Store;
use std::{path::PathBuf, time::Duration};

#[derive(Parser)]
struct Args {
    #[arg(long, env = "DATABASE_URL", hide_env_values = true)]
    database_url: String,
    /// Private operator credential file for the retained-output bucket.
    #[arg(long)]
    output_config: PathBuf,
    /// Perform one bounded discovery/cleanup tick, then exit.
    #[arg(long)]
    once: bool,
    /// Development only: permit retirement of explicitly simulated output.
    #[arg(long, default_value_t = false)]
    allow_simulated: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let retirer = S3Config::read_private(&args.output_config)?.build_retirer()?;
    let store = Store::connect(&args.database_url, 4).await?;
    store.migrate().await?;
    let cleaner = Cleaner::new(store, retirer, args.allow_simulated);
    if args.once {
        eprintln!("{:?}", cleaner.tick().await?);
        return Ok(());
    }
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => break,
            result = cleaner.tick() => match result {
                Ok(CleanupTick::Idle) => {},
                Ok(tick) => eprintln!("sandbox cleanup: {tick:?}"),
                Err(error) => eprintln!("sandbox cleanup: {error}"),
            }
        }
        tokio::select! {
            _ = tokio::signal::ctrl_c() => break,
            _ = tokio::time::sleep(Duration::from_millis(500)) => {},
        }
    }
    Ok(())
}
