//! Real loopback gRPC/TLS, with fresh ephemeral keys for each test.
#![allow(clippy::unwrap_used, clippy::expect_used)]
use rcgen::{
    BasicConstraints, Certificate, CertificateParams, CertifiedIssuer, ExtendedKeyUsagePurpose,
    IsCa, KeyPair, KeyUsagePurpose,
};
use sandbox_fake_host::{FakeConfig, FakeHost, unix_ms};
use sandbox_protocol::{
    AllocationId, HostId, Id, OperationId, ProjectId, SandboxId,
    supervisor::{
        CreateRequest, HealthRequest, Ownership, Resources, supervisor_client::SupervisorClient,
        supervisor_server::SupervisorServer,
    },
};
use sandbox_supervisor::transport::{self, ControllerIdentity, MAX_MESSAGE_BYTES};
use sha2::{Digest, Sha256};
use std::{collections::BTreeSet, time::Duration};
use tokio::{net::TcpListener, task::JoinHandle};
use tokio_stream::wrappers::TcpListenerStream;
use tonic::{
    Code,
    service::interceptor::InterceptedService,
    transport::{Certificate as TlsCertificate, ClientTlsConfig, Endpoint, Identity, Server},
};

struct Leaf {
    cert: Certificate,
    key: KeyPair,
}
impl Leaf {
    fn new(ca: &CertifiedIssuer<'_, KeyPair>, name: String, client: bool) -> Self {
        let key = KeyPair::generate().unwrap();
        let mut params = CertificateParams::new(vec![name]).unwrap();
        params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        params.extended_key_usages = vec![if client {
            ExtendedKeyUsagePurpose::ClientAuth
        } else {
            ExtendedKeyUsagePurpose::ServerAuth
        }];
        Self {
            cert: params.signed_by(&key, ca).unwrap(),
            key,
        }
    }
    fn pin(&self) -> [u8; 32] {
        Sha256::digest(self.cert.der()).into()
    }
}
fn ca() -> CertifiedIssuer<'static, KeyPair> {
    let mut params = CertificateParams::new(Vec::new()).unwrap();
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    CertifiedIssuer::self_signed(params, KeyPair::generate().unwrap()).unwrap()
}
struct Fixture {
    host: HostId,
    url: String,
    ca: CertifiedIssuer<'static, KeyPair>,
    controller: Leaf,
    rotated: Leaf,
    task: JoinHandle<()>,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Fixture {
    async fn new() -> Self {
        let host = HostId::generate();
        let ca = ca();
        let controller = Leaf::new(&ca, "controller.sandbox.internal".into(), true);
        let rotated = Leaf::new(&ca, "controller.sandbox.internal".into(), true);
        let server = Leaf::new(&ca, transport::host_server_name(host), false);
        let fake = FakeHost::new(FakeConfig {
            host,
            epoch: 1,
            images: BTreeSet::from([format!("sha256:{}", "a".repeat(64))]),
            capacity: Resources {
                vcpu: 4,
                memory_mib: 8192,
                disk_mib: 65536,
            },
        })
        .unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("https://{}", listener.local_addr().unwrap());
        let service = SupervisorServer::new(fake)
            .max_decoding_message_size(MAX_MESSAGE_BYTES)
            .max_encoding_message_size(MAX_MESSAGE_BYTES);
        let service = InterceptedService::new(
            service,
            ControllerIdentity::new(vec![controller.pin(), rotated.pin()]).unwrap(),
        );
        let tls = transport::server_tls(
            ca.pem().as_bytes(),
            server.cert.pem().as_bytes(),
            server.key.serialize_pem().as_bytes(),
        );
        let task = tokio::spawn(async move {
            Server::builder()
                .tls_config(tls)
                .unwrap()
                .add_service(service)
                .serve_with_incoming(TcpListenerStream::new(listener))
                .await
                .unwrap();
        });
        Self {
            host,
            url,
            ca,
            controller,
            rotated,
            task,
        }
    }
    async fn client(
        &self,
        leaf: &Leaf,
    ) -> Result<SupervisorClient<tonic::transport::Channel>, transport::TransportError> {
        transport::connect(
            &self.url,
            self.host,
            self.ca.pem().as_bytes(),
            leaf.cert.pem().as_bytes(),
            leaf.key.serialize_pem().as_bytes(),
        )
        .await
    }
}

#[tokio::test]
async fn configured_controller_and_rotation_certificate_can_use_real_rpc() {
    let fixture = Fixture::new().await;
    for leaf in [&fixture.controller, &fixture.rotated] {
        let mut client = fixture.client(leaf).await.unwrap();
        let info = client.health(HealthRequest {}).await.unwrap().into_inner();
        assert!(info.simulated);
        assert_eq!(info.host_id, fixture.host.to_string());
        assert_eq!(info.supervisor_epoch, 1);
        let response = client
            .create(CreateRequest {
                ownership: Some(Ownership {
                    host_id: fixture.host.to_string(),
                    project_id: ProjectId::generate().to_string(),
                    sandbox_id: SandboxId::generate().to_string(),
                    allocation_id: AllocationId::generate().to_string(),
                    operation_id: OperationId::generate().to_string(),
                    generation: 1,
                    supervisor_epoch: 1,
                    claim_revision: 1,
                    claim_expires_unix_ms: unix_ms().unwrap() + 30_000,
                }),
                image_digest: format!("sha256:{}", "a".repeat(64)),
                resources: Some(Resources {
                    vcpu: 1,
                    memory_mib: 512,
                    disk_mib: 1024,
                }),
                allocation_expires_unix_ms: unix_ms().unwrap() + 30_000,
            })
            .await
            .unwrap()
            .into_inner();
        assert!(response.simulated);
        assert_eq!(response.start_count, 1);
    }
}

#[tokio::test]
async fn same_ca_and_same_name_do_not_authorize_an_unconfigured_leaf() {
    let fixture = Fixture::new().await;
    let other = Leaf::new(&fixture.ca, "controller.sandbox.internal".into(), true);
    let mut client = fixture.client(&other).await.unwrap();
    assert_eq!(
        client.health(HealthRequest {}).await.unwrap_err().code(),
        Code::PermissionDenied
    );
}

#[tokio::test]
async fn untrusted_ca_cannot_authenticate_a_client() {
    let fixture = Fixture::new().await;
    let other = Leaf::new(&ca(), "controller.sandbox.internal".into(), true);
    if let Ok(mut client) = fixture.client(&other).await {
        assert!(client.health(HealthRequest {}).await.is_err());
    }
}

#[tokio::test]
async fn missing_client_certificate_is_rejected() {
    let fixture = Fixture::new().await;
    let endpoint = Endpoint::from_shared(fixture.url.clone())
        .unwrap()
        .timeout(Duration::from_secs(2))
        .connect_timeout(Duration::from_secs(2))
        .tls_config(
            ClientTlsConfig::new()
                .ca_certificate(TlsCertificate::from_pem(fixture.ca.pem()))
                .domain_name(transport::host_server_name(fixture.host)),
        )
        .unwrap();
    if let Ok(channel) = endpoint.connect().await {
        assert!(
            SupervisorClient::new(channel)
                .health(HealthRequest {})
                .await
                .is_err()
        );
    }
}

#[tokio::test]
async fn a_different_host_certificate_is_rejected() {
    let fixture = Fixture::new().await;
    assert!(
        transport::connect(
            &fixture.url,
            HostId::generate(),
            fixture.ca.pem().as_bytes(),
            fixture.controller.cert.pem().as_bytes(),
            fixture.controller.key.serialize_pem().as_bytes()
        )
        .await
        .is_err()
    );
}

#[tokio::test]
async fn plaintext_endpoint_has_no_client_bypass() {
    assert!(matches!(
        transport::connect("http://127.0.0.1:1", HostId::generate(), b"", b"", b"").await,
        Err(transport::TransportError::InsecureEndpoint)
    ));
    assert!(ControllerIdentity::new(vec![]).is_err());
    assert!(ControllerIdentity::new(vec![[0; 32]; 3]).is_err());
}

#[tokio::test]
async fn server_rejects_oversized_control_messages() {
    let fixture = Fixture::new().await;
    // Bypass the normal client's send bound to exercise the server's receive bound.
    let channel = Endpoint::from_shared(fixture.url.clone())
        .unwrap()
        .tls_config(
            ClientTlsConfig::new()
                .ca_certificate(TlsCertificate::from_pem(fixture.ca.pem()))
                .identity(Identity::from_pem(
                    fixture.controller.cert.pem(),
                    fixture.controller.key.serialize_pem(),
                ))
                .domain_name(transport::host_server_name(fixture.host)),
        )
        .unwrap()
        .connect()
        .await
        .unwrap();
    let mut client =
        SupervisorClient::new(channel).max_encoding_message_size(MAX_MESSAGE_BYTES * 2);
    let error = client
        .create(CreateRequest {
            image_digest: "a".repeat(MAX_MESSAGE_BYTES + 1),
            ..Default::default()
        })
        .await
        .unwrap_err();
    assert_eq!(error.code(), Code::OutOfRange);
}
