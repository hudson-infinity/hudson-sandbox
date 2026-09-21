//! Operator CLI and HTTPS entry point. There is no public bootstrap endpoint.
use anyhow::Context;
use clap::{Parser, Subcommand};
use sandbox_api::{
    AppState,
    provision::provision,
    router_with_output,
    server::{ServerLimits, serve, tls_acceptor},
};
use sandbox_protocol::images::ImageAllowlist;
use sandbox_store::Store;
use std::{net::SocketAddr, path::PathBuf};

#[derive(Parser)]
#[command(about = "Hudson Sandbox HTTPS API and offline operator setup")]
struct Args {
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    /// Serve existing routes. Requires DATABASE_URL in the environment.
    Serve {
        #[arg(long, default_value = "127.0.0.1:8443")]
        bind: SocketAddr,
        #[arg(long)]
        tls_cert: PathBuf,
        #[arg(long)]
        tls_key: PathBuf,
        #[arg(long, required = true)]
        image_digest: Vec<String>,
        /// Private service-owned S3 JSON configuration; provision read-only credentials.
        #[arg(long)]
        output_config: Option<PathBuf>,
    },
    /// Create or resume one project via a private credential file. Requires direct DATABASE_URL access.
    ProvisionProject {
        #[arg(long)]
        name: String,
        #[arg(long)]
        credential_file: PathBuf,
    },
    /// Apply embedded migrations without opening an HTTP listener.
    Migrate,
}
async fn store() -> anyhow::Result<Store> {
    let database_url =
        std::env::var("DATABASE_URL").map_err(|_| anyhow::anyhow!("DATABASE_URL is required"))?;
    let store = Store::connect(&database_url, 16)
        .await
        .map_err(|_| anyhow::anyhow!("invalid database configuration"))?;
    store
        .migrate()
        .await
        .map_err(|_| anyhow::anyhow!("database migration failed"))?;
    Ok(store)
}
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    // Fixed filters deliberately avoid wire-level debug logs containing request headers or SQL binds.
    tracing_subscriber::fmt()
        .with_target(false)
        .with_ansi(false)
        .with_max_level(tracing::Level::INFO)
        .with_writer(std::io::stderr)
        .init();
    match args.command {
        Command::Serve {
            bind,
            tls_cert,
            tls_key,
            image_digest,
            output_config,
        } => {
            let images = ImageAllowlist::new(image_digest)?;
            let acceptor = tls_acceptor(
                &std::fs::read(tls_cert).context("reading TLS certificate")?,
                &std::fs::read(tls_key).context("reading TLS key")?,
            )?;
            let shutdown = shutdown_signal()?;
            let store = store().await?;
            let output = load_output(output_config.as_deref())?;
            let listener = tokio::net::TcpListener::bind(bind)
                .await
                .context("binding HTTPS listener")?;
            tracing::info!(address = %listener.local_addr()?, "sandbox API listening over HTTPS");
            serve(
                listener,
                acceptor,
                router_with_output(AppState { store, images }, output),
                ServerLimits::default(),
                shutdown,
            )
            .await?;
        }
        Command::ProvisionProject {
            name,
            credential_file,
        } => {
            let store = store().await?;
            let project = provision(&store, &name, &credential_file).await?;
            // Only nonsecret metadata reaches stdout; the token lives exclusively in the private file.
            println!(
                "{}",
                serde_json::json!({"project_id":project,"credential_file":credential_file})
            );
        }
        Command::Migrate => {
            store().await?;
        }
    }
    Ok(())
}
#[cfg(unix)]
fn shutdown_signal() -> anyhow::Result<impl Future<Output = ()>> {
    use tokio::signal::unix::{SignalKind, signal};
    let mut interrupt = signal(SignalKind::interrupt()).context("registering SIGINT")?;
    let mut terminate = signal(SignalKind::terminate()).context("registering SIGTERM")?;
    Ok(async move {
        tokio::select! { _ = interrupt.recv() => {}, _ = terminate.recv() => {} }
    })
}
#[cfg(not(unix))]
fn shutdown_signal() -> anyhow::Result<impl Future<Output = ()>> {
    Ok(async {
        let _ = tokio::signal::ctrl_c().await;
    })
}

fn load_output(
    path: Option<&std::path::Path>,
) -> anyhow::Result<Option<std::sync::Arc<dyn sandbox_api::outputs::OutputReader>>> {
    let Some(path) = path else {
        return Ok(None);
    };
    #[cfg(unix)]
    {
        Ok(Some(std::sync::Arc::new(
            sandbox_artifacts::S3Config::read_private(path)?.build()?,
        )))
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        anyhow::bail!("private output configuration requires Unix")
    }
}
