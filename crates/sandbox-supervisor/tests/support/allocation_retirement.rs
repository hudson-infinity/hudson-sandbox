use super::*;
use sandbox_protocol::{
    allocation_authority::Permit,
    allocation_retirement::{DomainClosure, Intent, Request as RetirementRequest},
    supervisor::{AllocationAuthorityRequest, AllocationFenceRequest},
};

pub(super) fn permit(request: &CreateRequest) -> Permit {
    let o = request.ownership.as_ref().unwrap();
    Permit {
        host: o.host_id.parse().unwrap(),
        project: o.project_id.parse().unwrap(),
        sandbox: o.sandbox_id.parse().unwrap(),
        allocation: o.allocation_id.parse().unwrap(),
        create_operation: o.operation_id.parse().unwrap(),
        generation: o.generation,
        original_epoch: o.supervisor_epoch,
        serial: 1,
    }
}
pub(super) fn retirement(p: Permit) -> RetirementRequest {
    RetirementRequest {
        intent: Intent {
            version: 1,
            retirement: OperationId::generate(),
            permit: p,
            commands: DomainClosure::Empty {},
            files: DomainClosure::Empty {},
            release_evidence_sha256: "ab".repeat(32),
            simulated: false,
        },
        reporting_epoch: 1,
        revision: 1,
        expires_unix_ms: guardian::wall_ms() + 30000,
    }
}
fn wire(r: &RetirementRequest) -> AllocationFenceRequest {
    AllocationFenceRequest {
        request_json: r.encode().unwrap(),
    }
}
pub(super) async fn register(c: &mut SupervisorClient<Channel>, p: &Permit) {
    c.allocation_authority(AllocationAuthorityRequest {
        host_id: p.host.to_string(),
        reporting_epoch: 1,
        permits_json: serde_json::to_vec(std::slice::from_ref(p)).unwrap(),
    })
    .await
    .unwrap();
}
#[tokio::test]
#[ignore = "requires root, HUDSON_GUARDIAN_TEST_VM=1 and controlled KVM artifacts"]
async fn durable_retirement_fence_reconciles_failed_fence_and_lost_reply_across_restart() {
    let mut f = Fixture::configured_permits(true, false, true).await;
    let mut client = f.client().await;
    let create = f.request();
    let p = permit(&create);
    let mut request = retirement(p.clone());
    assert!(client.fence_allocation(wire(&request)).await.is_err());
    register(&mut client, &p).await;
    // An in-flight launch reader prevents the root fence, after the host has
    // durably retained its intent. This is not successful launch denial yet.
    let root = f.config.state_root.join("a");
    let launch = sandbox_supervisor::launch_authority::authorize(&root, Some(&p)).unwrap();
    assert_eq!(
        client
            .fence_allocation(wire(&request))
            .await
            .unwrap_err()
            .code(),
        Code::Unavailable
    );
    let journal: serde_json::Value =
        serde_json::from_slice(&fs::read(f.config.state_root.join("host.json")).unwrap()).unwrap();
    assert_eq!(
        journal["records"][p.allocation.to_string()]["retirement"],
        serde_json::to_value(&request).unwrap()
    );
    assert_eq!(
        client
            .inspect(InspectRequest {
                ownership: create.ownership.clone()
            })
            .await
            .unwrap_err()
            .code(),
        Code::FailedPrecondition
    );
    let mut changed = request.clone();
    changed.intent.release_evidence_sha256 = "cd".repeat(32);
    changed.revision += 1;
    assert_eq!(
        client
            .fence_allocation(wire(&changed))
            .await
            .unwrap_err()
            .code(),
        Code::FailedPrecondition
    );
    drop(launch);
    let expected = wire(&request);
    client.fence_allocation(expected.clone()).await.unwrap(); // discard acknowledgement
    assert_eq!(
        client
            .fence_allocation(expected.clone())
            .await
            .unwrap()
            .get_ref()
            .request
            .as_ref(),
        Some(&expected)
    );
    assert!(sandbox_supervisor::launch_authority::authorize(&root, Some(&p)).is_err());
    assert!(
        client
            .stop(StopRequest {
                ownership: create.ownership.clone()
            })
            .await
            .is_err()
    );
    let mut changed_claim = request.clone();
    changed_claim.expires_unix_ms += 1;
    assert!(client.fence_allocation(wire(&changed_claim)).await.is_err());
    f.restart().await;
    client = f.client().await;
    assert!(client.fence_allocation(wire(&request)).await.is_err());
    request.reporting_epoch = 2;
    request.revision = 2;
    request.expires_unix_ms = guardian::wall_ms() + 30000;
    client.fence_allocation(wire(&request)).await.unwrap();
    assert!(sandbox_supervisor::launch_authority::authorize(&root, Some(&p)).is_err());
    assert!(f.config.state_root.join("host.json").exists());
}
#[tokio::test]
#[ignore = "requires root, HUDSON_GUARDIAN_TEST_VM=1 and controlled KVM artifacts"]
async fn real_stopped_vm_can_be_fenced_without_deleting_its_cleanup_evidence() {
    let mut f = Fixture::configured_permits(true, false, true).await;
    let mut client = f.client().await;
    let mut create = f.request();
    let p = permit(&create);
    register(&mut client, &p).await;
    create.launch_permit_json = serde_json::to_vec(&p).unwrap();
    if let Err(error) = client.create(create.clone()).await {
        assert!(
            matches!(error.code(), Code::Cancelled | Code::DeadlineExceeded),
            "{error}"
        );
    }
    let owner = create.ownership.as_ref().unwrap().clone();
    assert_eq!(f.ready(&mut client, &owner).await.start_count, 1);
    let mut request = retirement(p.clone());
    assert_eq!(
        client
            .fence_allocation(wire(&request))
            .await
            .unwrap_err()
            .code(),
        Code::FailedPrecondition
    );
    // Even a capture whose guest response fails owns a pending host ticket.
    let scope = sandbox_protocol::file_downloads::ReadScope {
        version: 1,
        host_id: p.host,
        host_epoch: 1,
        project_id: p.project,
        sandbox_id: p.sandbox,
        allocation_id: p.allocation,
        generation: p.generation,
    };
    let mut reader = transport::connect_file_reader(
        &f.url,
        f.config.host,
        f.tls.ca.pem().as_bytes(),
        f.file_reader.cert.pem().as_bytes(),
        f.file_reader.key.serialize_pem().as_bytes(),
    )
    .await
    .unwrap();
    assert_eq!(
        reader
            .capture(sandbox_protocol::supervisor::FileCaptureRequest {
                scope_json: serde_json::to_vec(&scope).unwrap(),
                path: "never-created.txt".into(),
                expires_unix_ms: guardian::wall_ms() + 30000,
            })
            .await
            .unwrap_err()
            .code(),
        Code::Unavailable
    );
    let mut stop = owner.clone();
    stop.operation_id = OperationId::generate().to_string();
    if let Err(error) = client
        .stop(StopRequest {
            ownership: Some(stop.clone()),
        })
        .await
    {
        assert!(
            matches!(
                error.code(),
                Code::Unavailable | Code::Cancelled | Code::DeadlineExceeded
            ),
            "{error}"
        );
    }
    f.released(&mut client, &stop).await;
    request.expires_unix_ms = guardian::wall_ms() + 120000;
    assert_eq!(
        client
            .fence_allocation(wire(&request))
            .await
            .unwrap_err()
            .code(),
        Code::Unavailable
    );
    assert!(
        sandbox_supervisor::launch_authority::authorize(&f.config.state_root.join("a"), Some(&p))
            .is_err()
    );
    // This failed handoff retains intent and cannot be treated as release.
    assert_eq!(
        client
            .inspect(InspectRequest {
                ownership: Some(owner)
            })
            .await
            .unwrap_err()
            .code(),
        Code::FailedPrecondition
    );
    tokio::time::sleep(sandbox_supervisor::file_downloads::TTL + Duration::from_millis(100)).await;
    client.fence_allocation(wire(&request)).await.unwrap();
    let journal: serde_json::Value =
        serde_json::from_slice(&fs::read(f.config.state_root.join("host.json")).unwrap()).unwrap();
    let manifest: Manifest =
        serde_json::from_value(journal["records"][p.allocation.to_string()]["manifest"].clone())
            .unwrap();
    assert!(manifest.receipt().unwrap().cleanup_confirmed);
    assert!(
        sandbox_supervisor::launch_authority::authorize(&f.config.state_root.join("a"), Some(&p))
            .is_err()
    );
    f.restart().await;
    client = f.client().await;
    request.reporting_epoch = 2;
    request.revision = 2;
    request.expires_unix_ms = guardian::wall_ms() + 30000;
    client.fence_allocation(wire(&request)).await.unwrap();
    assert!(manifest.receipt().unwrap().cleanup_confirmed);
}
