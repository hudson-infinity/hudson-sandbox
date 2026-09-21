//! Mutual TLS plus explicit service identity. A CA-valid peer alone is insufficient.

use std::time::Duration;

use sandbox_protocol::{HostId, Id, supervisor::supervisor_client::SupervisorClient};
use sha2::{Digest, Sha256};
use tonic::{
    Request, Status,
    service::Interceptor,
    transport::{Certificate, Channel, ClientTlsConfig, Endpoint, Identity, ServerTlsConfig},
};

/// The internal control messages contain metadata only, never guest output.
pub const MAX_MESSAGE_BYTES: usize = 64 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    #[error("configure one or two controller certificate fingerprints")]
    InvalidPins,
    #[error("supervisor endpoint must use https")]
    InsecureEndpoint,
    #[error("supervisor transport failed: {0}")]
    Transport(#[from] tonic::transport::Error),
}

/// SHA-256 fingerprints of the exact permitted controller leaf certificates.
/// Two pins permit controlled certificate rotation; unrelated CA peers are denied.
#[derive(Debug, Clone)]
pub struct ControllerIdentity {
    pins: Vec<[u8; 32]>,
}

impl ControllerIdentity {
    pub fn new(pins: Vec<[u8; 32]>) -> Result<Self, TransportError> {
        if pins.is_empty() || pins.len() > 2 {
            return Err(TransportError::InvalidPins);
        }
        Ok(Self { pins })
    }
}

impl Interceptor for ControllerIdentity {
    fn call(&mut self, request: Request<()>) -> Result<Request<()>, Status> {
        let certs = request
            .peer_certs()
            .ok_or_else(|| Status::unauthenticated("client certificate required"))?;
        let leaf = certs
            .first()
            .ok_or_else(|| Status::unauthenticated("client certificate required"))?;
        let fingerprint: [u8; 32] = Sha256::digest(leaf.as_ref()).into();
        if !self.pins.contains(&fingerprint) {
            return Err(Status::permission_denied(
                "controller identity not authorized",
            ));
        }
        Ok(request)
    }
}

#[must_use]
pub fn server_tls(ca_pem: &[u8], cert_pem: &[u8], key_pem: &[u8]) -> ServerTlsConfig {
    ServerTlsConfig::new()
        .identity(Identity::from_pem(cert_pem, key_pem))
        .client_ca_root(Certificate::from_pem(ca_pem))
        .client_auth_optional(false)
        .timeout(Duration::from_secs(5))
}

/// Stable per-host DNS identity in its server certificate's subjectAltName.
#[must_use]
pub fn host_server_name(host: HostId) -> String {
    format!("host-{}.sandbox.internal", host.uuid())
}

/// There is no plaintext or certificate-verification bypass in this client.
pub async fn connect(
    endpoint: &str,
    host: HostId,
    ca_pem: &[u8],
    cert_pem: &[u8],
    key_pem: &[u8],
) -> Result<SupervisorClient<Channel>, TransportError> {
    let endpoint = Endpoint::from_shared(endpoint.to_owned())?;
    if endpoint.uri().scheme_str() != Some("https") {
        return Err(TransportError::InsecureEndpoint);
    }
    let channel = endpoint
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(5))
        .tls_config(
            ClientTlsConfig::new()
                .ca_certificate(Certificate::from_pem(ca_pem))
                .identity(Identity::from_pem(cert_pem, key_pem))
                .domain_name(host_server_name(host)),
        )?
        .connect()
        .await?;
    Ok(SupervisorClient::new(channel)
        .max_decoding_message_size(MAX_MESSAGE_BYTES)
        .max_encoding_message_size(MAX_MESSAGE_BYTES))
}
