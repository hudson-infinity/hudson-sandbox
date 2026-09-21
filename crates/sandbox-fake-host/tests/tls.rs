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
    reader: Leaf,
    reader_rotated: Leaf,
    fake: FakeHost,
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
        let reader = Leaf::new(&ca, "reader.sandbox.internal".into(), true);
        let reader_rotated = Leaf::new(&ca, "reader.sandbox.internal".into(), true);
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
        let service = SupervisorServer::new(fake.clone())
            .max_decoding_message_size(MAX_MESSAGE_BYTES)
            .max_encoding_message_size(MAX_MESSAGE_BYTES);
        let identity = ControllerIdentity::new(vec![controller.pin(), rotated.pin()]).unwrap();
        let reader_service = InterceptedService::new(
            sandbox_protocol::supervisor::live_output_server::LiveOutputServer::new(fake.clone())
                .max_decoding_message_size(MAX_MESSAGE_BYTES)
                .max_encoding_message_size(MAX_MESSAGE_BYTES),
            ControllerIdentity::output_reader(vec![reader.pin(), reader_rotated.pin()], &identity)
                .unwrap(),
        );
        let service = InterceptedService::new(service, identity);
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
                .add_service(reader_service)
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
            reader,
            reader_rotated,
            fake,
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

impl Fixture {
    async fn reader(
        &self,
        leaf: &Leaf,
    ) -> sandbox_protocol::supervisor::live_output_client::LiveOutputClient<tonic::transport::Channel>
    {
        transport::connect_output_reader(
            &self.url,
            self.host,
            self.ca.pem().as_bytes(),
            leaf.cert.pem().as_bytes(),
            leaf.key.serialize_pem().as_bytes(),
        )
        .await
        .unwrap()
    }
    async fn live_command(
        &self,
    ) -> (
        sandbox_protocol::supervisor::CommandRequest,
        sandbox_protocol::supervisor::LiveOutputRequest,
    ) {
        use sandbox_protocol::{
            guest_model as m, live_output::LiveOutputScope, output::OutputOwner, supervisor::*,
        };
        let mut c = self.client(&self.controller).await.unwrap();
        let mut owner = Ownership {
            host_id: self.host.to_string(),
            project_id: ProjectId::generate().to_string(),
            sandbox_id: SandboxId::generate().to_string(),
            allocation_id: AllocationId::generate().to_string(),
            operation_id: OperationId::generate().to_string(),
            generation: 1,
            supervisor_epoch: 1,
            claim_revision: 1,
            claim_expires_unix_ms: unix_ms().unwrap() + 30000,
        };
        c.create(CreateRequest {
            ownership: Some(owner.clone()),
            image_digest: format!("sha256:{}", "a".repeat(64)),
            resources: Some(Resources {
                vcpu: 1,
                memory_mib: 128,
                disk_mib: 64,
            }),
            allocation_expires_unix_ms: unix_ms().unwrap() + 30000,
        })
        .await
        .unwrap();
        owner.operation_id = OperationId::generate().to_string();
        let command = m::Execute {
            operation_id: owner.operation_id.parse().unwrap(),
            argv: vec!["/simulated".into()],
            env: Default::default(),
            cwd: "/".into(),
            deadline_unix_ms: unix_ms().unwrap() + 20000,
            output_limit: 1024,
        };
        let request = CommandRequest {
            ownership: Some(owner.clone()),
            command: Some((&command).into()),
        };
        self.fake.hold_next_command().await;
        let receipt: m::Receipt = c
            .execute_command(request.clone())
            .await
            .unwrap()
            .into_inner()
            .receipt
            .unwrap()
            .try_into()
            .unwrap();
        let scope = LiveOutputScope {
            version: 1,
            owner: OutputOwner {
                project_id: owner.project_id.parse().unwrap(),
                sandbox_id: owner.sandbox_id.parse().unwrap(),
                operation_id: command.operation_id,
                allocation_id: receipt.context.allocation_id,
                generation: receipt.context.generation,
                boot_id: receipt.context.boot_id,
                host_id: self.host,
                host_epoch: 1,
            },
            command_digest: command.digest().unwrap(),
            deadline_unix_ms: command.deadline_unix_ms,
            output_limit: command.output_limit,
        };
        (
            request,
            LiveOutputRequest {
                scope_json: serde_json::to_vec(&scope).unwrap(),
                output: Some(sandbox_protocol::guest::ReadOutput {
                    operation_id: command.operation_id.to_string(),
                    stream: sandbox_protocol::guest::Stream::Stdout as i32,
                    offset: 0,
                    limit: 32,
                }),
                expires_unix_ms: unix_ms().unwrap() + 5000,
            },
        )
    }
}

#[tokio::test]
async fn live_reader_rotation_and_controller_roles_are_disjoint_on_every_rpc() {
    use sandbox_protocol::supervisor::*;
    let f = Fixture::new().await;
    let other = Leaf::new(&f.ca, "reader.sandbox.internal".into(), true);
    for leaf in [&f.controller, &f.rotated, &other] {
        assert_eq!(
            f.reader(leaf)
                .await
                .read(LiveOutputRequest::default())
                .await
                .unwrap_err()
                .code(),
            Code::PermissionDenied
        );
    }
    for leaf in [&f.reader, &f.reader_rotated] {
        assert_eq!(
            f.reader(leaf)
                .await
                .read(LiveOutputRequest::default())
                .await
                .unwrap_err()
                .code(),
            Code::InvalidArgument
        );
        let mut c = f.client(leaf).await.unwrap();
        macro_rules! denied {
            ($method:ident,$request:expr) => {
                assert_eq!(
                    c.$method($request).await.unwrap_err().code(),
                    Code::PermissionDenied
                )
            };
        }
        denied!(create, CreateRequest::default());
        denied!(stop, StopRequest::default());
        denied!(inspect, InspectRequest::default());
        denied!(health, HealthRequest::default());
        denied!(execute_command, CommandRequest::default());
        denied!(inspect_command, CommandInspection::default());
        denied!(prepare_output, OutputRequest::default());
        denied!(archive_output, OutputRequest::default());
        denied!(renew_lease, LeaseRequest::default());
        denied!(inspect_lease, LeaseInspection::default());
    }
    let controller = ControllerIdentity::new(vec![f.controller.pin(), f.rotated.pin()]).unwrap();
    assert!(
        ControllerIdentity::output_reader(vec![f.reader.pin(), f.rotated.pin()], &controller)
            .is_err()
    );
    assert!(ControllerIdentity::output_reader(vec![], &controller).is_err());
    assert!(ControllerIdentity::output_reader(vec![f.reader.pin(); 3], &controller).is_err());
    assert_eq!(f.fake.total_starts().await, 0);
    assert_eq!(f.fake.total_commands().await, 0);
}

#[tokio::test]
async fn live_reads_do_not_execute_fence_or_misrepresent_missing_and_pending_output() {
    use sandbox_protocol::{guest_model as m, live_output::LiveOutputScope, supervisor::*};
    let f = Fixture::new().await;
    let (command, read) = f.live_command().await;
    let mut reader = f.reader(&f.reader).await;
    let pending = reader.read(read.clone()).await.unwrap().into_inner();
    assert!(pending.simulated);
    assert_eq!(pending.request, Some(read.clone()));
    let chunk = pending.chunk.unwrap();
    assert!(chunk.data.is_empty() && chunk.at_end && !chunk.complete);
    let id: OperationId = command
        .ownership
        .as_ref()
        .unwrap()
        .operation_id
        .parse()
        .unwrap();
    f.fake.finish_command(id, 7).await.unwrap();
    for leaf in [&f.reader, &f.reader_rotated] {
        let finished = f
            .reader(leaf)
            .await
            .read(read.clone())
            .await
            .unwrap()
            .into_inner();
        assert!(finished.chunk.unwrap().complete);
        assert_eq!(finished.receipt.unwrap().exit_code, Some(7));
    }
    assert_eq!(f.fake.total_commands().await, 1);
    // Reading absence cannot install the no-start fence that controller InspectCommand installs.
    let mut absent: LiveOutputScope = serde_json::from_slice(&read.scope_json).unwrap();
    absent.owner.operation_id = OperationId::generate();
    let mut next: m::Execute = command.command.clone().unwrap().try_into().unwrap();
    next.operation_id = absent.owner.operation_id;
    absent.command_digest = next.digest().unwrap();
    let mut missing = read.clone();
    missing.scope_json = serde_json::to_vec(&absent).unwrap();
    missing.output.as_mut().unwrap().operation_id = next.operation_id.to_string();
    assert_eq!(
        reader.read(missing.clone()).await.unwrap_err().code(),
        Code::NotFound
    );
    let mut owner = command.ownership.clone().unwrap();
    owner.operation_id = next.operation_id.to_string();
    let mut controller = f.client(&f.controller).await.unwrap();
    let result = controller
        .execute_command(CommandRequest {
            ownership: Some(owner),
            command: Some((&next).into()),
        })
        .await
        .unwrap()
        .into_inner();
    assert!(!result.not_started);
    assert_eq!(f.fake.total_commands().await, 2);
    assert!(
        reader
            .read(missing)
            .await
            .unwrap()
            .into_inner()
            .chunk
            .unwrap()
            .complete
    );
    let mut expired = read.clone();
    expired.expires_unix_ms = 1;
    assert_eq!(
        reader.read(expired).await.unwrap_err().code(),
        Code::InvalidArgument
    );
    for field in 0..10 {
        let mut scope: LiveOutputScope = serde_json::from_slice(&read.scope_json).unwrap();
        match field {
            0 => scope.owner.project_id = ProjectId::generate(),
            1 => scope.owner.sandbox_id = SandboxId::generate(),
            2 => scope.owner.allocation_id = AllocationId::generate(),
            3 => scope.owner.generation += 1,
            4 => scope.owner.boot_id.push('x'),
            5 => scope.owner.host_id = HostId::generate(),
            6 => scope.owner.host_epoch += 1,
            7 => scope.command_digest[0] ^= 1,
            8 => scope.output_limit += 1,
            _ => scope.deadline_unix_ms += 1,
        }
        let mut r = read.clone();
        r.scope_json = serde_json::to_vec(&scope).unwrap();
        assert!(reader.read(r).await.is_err(), "field {field}");
    }
    let mut past = read.clone();
    past.output.as_mut().unwrap().offset = 1;
    assert_eq!(
        reader.read(past).await.unwrap_err().code(),
        Code::OutOfRange
    );
    let mut owner = command.ownership.unwrap();
    owner.operation_id = OperationId::generate().to_string();
    controller
        .stop(StopRequest {
            ownership: Some(owner),
        })
        .await
        .unwrap();
    assert_eq!(
        reader.read(read).await.unwrap_err().code(),
        Code::Unavailable
    );
    assert_eq!(f.fake.total_commands().await, 2);
}
