//! TLS-only HTTP/1.1 transport. Connection bounds include incomplete handshakes.
use std::{future::Future, sync::Arc, time::Duration};

use anyhow::{Context, ensure};
use axum::{
    Router,
    extract::{DefaultBodyLimit, Request, State},
    middleware::{self, Next},
    response::{IntoResponse, Response},
};
use hyper_util::{
    rt::{TokioIo, TokioTimer},
    service::TowerToHyperService,
};
use tokio::{
    net::TcpListener,
    sync::{Semaphore, watch},
    task::JoinSet,
    time::timeout,
};
use tokio_rustls::{
    TlsAcceptor,
    rustls::{
        self,
        pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject},
    },
};

use crate::problem::Problem;

/// Operator-independent bounds for the initial non-streaming API.
#[derive(Debug, Clone, Copy)]
pub struct ServerLimits {
    pub connections: usize,
    pub handshake: Duration,
    pub headers: Duration,
    pub request: Duration,
    pub connection: Duration,
    pub drain: Duration,
}
impl Default for ServerLimits {
    fn default() -> Self {
        Self {
            connections: 128,
            handshake: Duration::from_secs(5),
            headers: Duration::from_secs(10),
            request: Duration::from_secs(30),
            connection: Duration::from_secs(120),
            drain: Duration::from_secs(10),
        }
    }
}
impl ServerLimits {
    fn validate(self) -> anyhow::Result<()> {
        ensure!(
            (1..=4096).contains(&self.connections),
            "invalid connection limit"
        );
        for duration in [
            self.handshake,
            self.headers,
            self.request,
            self.connection,
            self.drain,
        ] {
            ensure!(
                !duration.is_zero() && duration <= Duration::from_secs(3600),
                "invalid transport deadline"
            );
        }
        Ok(())
    }
}

/// Parse certificate/key and reject missing, malformed, or mismatched material before binding.
/// PEM contents never appear in errors. Certificate trust and hostname validation belong to clients.
pub fn tls_acceptor(cert: &[u8], key: &[u8]) -> anyhow::Result<TlsAcceptor> {
    let certs = CertificateDer::pem_slice_iter(cert)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| anyhow::anyhow!("invalid TLS certificate PEM"))?;
    ensure!(!certs.is_empty(), "TLS certificate chain is empty");
    let key = PrivateKeyDer::from_pem_slice(key)
        .map_err(|_| anyhow::anyhow!("invalid TLS private key PEM"))?;
    let mut config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .context("TLS protocol configuration")?
    .with_no_client_auth()
    .with_single_cert(certs, key)
    .map_err(|_| anyhow::anyhow!("invalid or mismatched TLS certificate and key"))?;
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(TlsAcceptor::from(Arc::new(config)))
}

async fn request_deadline(
    State(duration): State<Duration>,
    request: Request,
    next: Next,
) -> Response {
    match timeout(duration, next.run(request)).await {
        Ok(response) => response,
        Err(_) => Problem::Unavailable.into_response(),
    }
}

/// Stop accepting immediately on shutdown, drain active requests, then abort remaining connections.
/// No request paths, headers, credentials, query strings, or TLS errors are logged by transport.
pub async fn serve(
    listener: TcpListener,
    acceptor: TlsAcceptor,
    app: Router,
    limits: ServerLimits,
    shutdown: impl Future<Output = ()> + Send,
) -> anyhow::Result<()> {
    limits.validate()?;
    let app = app
        .layer(DefaultBodyLimit::max(64 * 1024))
        .layer(middleware::from_fn_with_state(
            limits.request,
            request_deadline,
        ));
    let slots = Arc::new(Semaphore::new(limits.connections));
    let (stop, stopping) = watch::channel(false);
    let mut tasks = JoinSet::new();
    tokio::pin!(shutdown);
    let result = loop {
        tokio::select! {
            biased;
            _ = &mut shutdown => break Ok(()),
            Some(result) = tasks.join_next(), if !tasks.is_empty() => {
                if result.is_err() { tracing::warn!("API connection task failed"); }
            }
            accepted = listener.accept() => {
                let (socket, _) = match accepted { Ok(socket) => socket, Err(_) => break Err(anyhow::anyhow!("API listener failed")) };
                // Do not queue unbounded sockets or tasks while every slot is occupied.
                let Ok(permit) = slots.clone().try_acquire_owned() else { continue };
                let acceptor = acceptor.clone();
                let app = app.clone();
                let mut stopping = stopping.clone();
                tasks.spawn(async move {
                    let _permit = permit;
                    let work = async {
                        let tls = tokio::select! {
                            biased;
                            _ = stopping.changed() => return,
                            result = timeout(limits.handshake, acceptor.accept(socket)) => match result { Ok(Ok(tls)) => tls, _ => return },
                        };
                        let mut builder = hyper::server::conn::http1::Builder::new();
                        builder.timer(TokioTimer::new()).header_read_timeout(limits.headers).max_headers(64).max_buf_size(32 * 1024);
                        let connection = builder.serve_connection(TokioIo::new(tls), TowerToHyperService::new(app));
                        tokio::pin!(connection);
                        tokio::select! {
                            biased;
                            _ = stopping.changed() => {
                                connection.as_mut().graceful_shutdown();
                                let _ = connection.await;
                            }
                            _ = &mut connection => {}
                        }
                    };
                    let _ = timeout(limits.connection, work).await;
                });
            }
        }
    };
    drop(listener);
    let _ = stop.send(true);
    if timeout(limits.drain, async {
        while tasks.join_next().await.is_some() {}
    })
    .await
    .is_err()
    {
        tasks.abort_all();
        while tasks.join_next().await.is_some() {}
    }
    result
}
