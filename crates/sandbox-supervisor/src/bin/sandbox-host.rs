//! Root-operated real Linux lifecycle service. Every network call requires mTLS.
#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("sandbox-host requires Linux");
    std::process::exit(1);
}
#[cfg(target_os = "linux")]
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    use clap::Parser;
    use sandbox_protocol::supervisor::supervisor_server::SupervisorServer;
    use sandbox_supervisor::{
        guardian,
        host::{Config, Host},
        transport::{ControllerIdentity, MAX_MESSAGE_BYTES, server_tls},
    };
    use std::{net::SocketAddr, path::PathBuf, time::Duration};
    #[derive(Parser)]
    struct Args {
        #[arg(long)]
        config: PathBuf,
        #[arg(long, default_value = "127.0.0.1:7443")]
        listen: SocketAddr,
        #[arg(long)]
        ca_cert: PathBuf,
        #[arg(long)]
        server_cert: PathBuf,
        #[arg(long)]
        server_key: PathBuf,
        /// SHA-256 of an authorized controller's DER leaf certificate. Repeat for rotation.
        #[arg(long, required = true)]
        controller_cert_sha256: Vec<String>,
    }
    let args = Args::parse();
    let pins = args
        .controller_cert_sha256
        .iter()
        .map(|p| {
            let mut bytes = [0u8; 32];
            hex::decode_to_slice(p, &mut bytes)?;
            Ok(bytes)
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    let identity = ControllerIdentity::new(pins)?;
    let tls = server_tls(
        &std::fs::read(args.ca_cert)?,
        &std::fs::read(args.server_cert)?,
        &std::fs::read(args.server_key)?,
    );
    let config: Config = guardian::read_json(&args.config)?;
    let host = Host::open(config)?;
    let service = SupervisorServer::new(host.clone())
        .max_decoding_message_size(MAX_MESSAGE_BYTES)
        .max_encoding_message_size(MAX_MESSAGE_BYTES);
    let service = tonic::service::interceptor::InterceptedService::new(service, identity);
    let maintenance = tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_millis(100));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            let _ = host.reconcile_one().await;
        }
    });
    eprintln!("real supervisor listening on {}", args.listen);
    let result = tonic::transport::Server::builder()
        .tls_config(tls)?
        .add_service(service)
        .serve_with_shutdown(args.listen, async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await;
    maintenance.abort();
    result.map_err(Into::into)
}
