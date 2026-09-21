//! Opt-in cleanup service. Credentials and deployment are operator-owned.
use clap::Parser;
use sandbox_artifacts::S3Config;
use sandbox_cleanup::{Cleaner, CleanupTick};
use sandbox_store::Store;
use sandbox_store::retention::ResponseRetention;
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
    /// Opt in to terminal response expiry, measured from completion. Applies
    /// to existing completed work too; never changes already assigned deadlines.
    #[arg(long, value_parser = clap::value_parser!(u32).range(1..=31_536_000))]
    response_retention_seconds: Option<u32>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let retirer = S3Config::read_private(&args.output_config)?.build_retirer()?;
    let store = Store::connect(&args.database_url, 4)
        .await
        .map_err(|_| anyhow::anyhow!("invalid database configuration"))?;
    store
        .migrate()
        .await
        .map_err(|_| anyhow::anyhow!("database migration failed"))?;
    let mut cleaner = Cleaner::new(store, retirer, args.allow_simulated);
    if let Some(seconds) = args.response_retention_seconds {
        cleaner = cleaner.with_response_retention(
            ResponseRetention::new(seconds)
                .ok_or_else(|| anyhow::anyhow!("invalid response retention policy"))?,
        );
    }
    if args.once {
        // Preserve the redacted Display message without attaching provider
        // error sources, which anyhow would otherwise print at process exit.
        let tick = cleaner
            .tick()
            .await
            .map_err(|error| anyhow::anyhow!("{error}"))?;
        eprintln!("{tick:?}");
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
