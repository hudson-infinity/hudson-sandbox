use super::allocation_retirement::{permit, register, retirement};
use super::*;
use sandbox_protocol::{
    allocation_retirement::{DomainClosure, Request as RetirementRequest},
    history::Domain,
    supervisor::AllocationMetadataRequest,
};
fn wire(r: &RetirementRequest) -> AllocationMetadataRequest {
    AllocationMetadataRequest {
        request_json: r.encode().unwrap(),
    }
}
#[tokio::test]
#[ignore = "requires root, HUDSON_GUARDIAN_TEST_VM=1 and controlled KVM artifacts"]
async fn unused_registered_permit_metadata_completion_survives_restart_without_inventing_release() {
    let mut f = Fixture::configured_permits(true, false, true).await;
    let mut client = f.client().await;
    let create = f.request();
    let p = permit(&create);
    let mut request = retirement(p.clone());
    assert!(
        client
            .retire_allocation_metadata(wire(&request))
            .await
            .is_err()
    );
    register(&mut client, &p).await;
    let expected = wire(&request);
    let observation = client
        .retire_allocation_metadata(expected.clone())
        .await
        .unwrap()
        .into_inner();
    assert_eq!(observation.request, Some(expected.clone()));
    assert!(observation.observed_unix_ms > 0);
    assert_eq!(
        client
            .retire_allocation_metadata(expected)
            .await
            .unwrap()
            .get_ref()
            .request,
        Some(wire(&request))
    );
    let path = f.config.state_root.join("host.json");
    let journal: serde_json::Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
    let record = &journal["records"][p.allocation.to_string()];
    assert_eq!(record["released"], false);
    assert_eq!(record["metadata_retirement"]["removed"], true);
    assert!(record["manifest"].is_null());
    assert!(
        !f.config
            .state_root
            .join("a")
            .join(p.allocation.to_string())
            .exists()
    );
    f.restart().await;
    client = f.client().await;
    assert!(
        client
            .retire_allocation_metadata(wire(&request))
            .await
            .is_err()
    );
    request.reporting_epoch = 2;
    request.revision += 1;
    request.expires_unix_ms = guardian::wall_ms() + 30000;
    client
        .retire_allocation_metadata(wire(&request))
        .await
        .unwrap();
    let mut changed = request.clone();
    changed.intent.release_evidence_sha256 = "ef".repeat(32);
    assert!(
        client
            .retire_allocation_metadata(wire(&changed))
            .await
            .is_err()
    );
    assert_eq!(
        client
            .inspect(InspectRequest {
                ownership: create.ownership
            })
            .await
            .unwrap_err()
            .code(),
        Code::FailedPrecondition
    );
}
#[tokio::test]
#[ignore = "requires root, HUDSON_GUARDIAN_TEST_VM=1 and controlled KVM artifacts"]
async fn real_metadata_deletion_preserves_history_proofs_and_recovers_lost_completion() {
    let mut f = Fixture::configured_permits(true, false, true).await;
    let mut client = f.client().await;
    let mut create = f.request();
    let p = permit(&create);
    register(&mut client, &p).await;
    create.launch_permit_json = serde_json::to_vec(&p).unwrap();
    let owner = create.ownership.clone().unwrap();
    let _ = client.create(create.clone()).await;
    assert_eq!(f.ready(&mut client, &owner).await.start_count, 1);
    let manifest = f.manifest(&owner);
    let guest = manifest.guest_client().unwrap();
    let (command, receipt) = super::history::execute(&mut client, &owner).await;
    let history = super::history::request(
        &owner,
        guest.context(),
        Domain::Commands,
        receipt.operation_id,
        1,
    );
    client.retire_history(history).await.unwrap();
    let mut request = retirement(p.clone());
    request.intent.commands = DomainClosure::Retired {
        through: receipt.operation_id,
    };
    assert_eq!(
        client
            .retire_allocation_metadata(wire(&request))
            .await
            .unwrap_err()
            .code(),
        Code::FailedPrecondition
    );
    let mut stop = owner.clone();
    stop.operation_id = OperationId::generate().to_string();
    let _ = client
        .stop(StopRequest {
            ownership: Some(stop.clone()),
        })
        .await;
    f.released(&mut client, &stop).await;
    let unknown = manifest.directory().join("unowned");
    fs::write(&unknown, b"preserve").unwrap();
    request.expires_unix_ms = guardian::wall_ms() + 30000;
    assert_eq!(
        client
            .retire_allocation_metadata(wire(&request))
            .await
            .unwrap_err()
            .code(),
        Code::Unavailable
    );
    assert_eq!(fs::read(&unknown).unwrap(), b"preserve");
    assert!(manifest.receipt().unwrap().cleanup_confirmed);
    fs::remove_file(unknown).unwrap();
    let expected = wire(&request);
    client
        .retire_allocation_metadata(expected.clone())
        .await
        .unwrap(); // discard acknowledgement
    assert!(!manifest.directory().exists());
    assert!(!manifest.group().exists());
    assert_eq!(
        client
            .retire_allocation_metadata(expected)
            .await
            .unwrap()
            .get_ref()
            .request,
        Some(wire(&request))
    );
    assert!(client.execute_command(command).await.is_err());
    assert!(manifest.prepare().is_err());
    assert!(manifest.reconcile("late_wrapper").is_err());
    assert!(manifest.fence_unstarted().is_err());
    assert!(!manifest.directory().exists());
    // Simulate the retained prepared journal after deletion but before its final
    // completion save. The guardian unit tests exercise every actual unlink step.
    let allocation = p.allocation.to_string();
    f.restart_with(|f| {
        let path = f.config.state_root.join("host.json");
        let mut journal: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        journal["records"][&allocation]["metadata_retirement"]["removed"] = false.into();
        fs::write(&path, serde_json::to_vec(&journal).unwrap()).unwrap();
        fs::File::open(path).unwrap().sync_all().unwrap();
    })
    .await;
    client = f.client().await;
    request.reporting_epoch = 2;
    request.revision += 1;
    request.expires_unix_ms = guardian::wall_ms() + 30000;
    client
        .retire_allocation_metadata(wire(&request))
        .await
        .unwrap();
    assert!(!manifest.directory().exists());
    // Corrupt retained cleanup is not a reason to bypass missing original files.
    f.child.kill().unwrap();
    f.child.wait().unwrap();
    let path = f.config.state_root.join("host.json");
    let bytes = fs::read(&path).unwrap();
    let original: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let mut next_config = f.config.clone();
    next_config.epoch += 1;
    for field in ["scope", "receipt", "intent"] {
        let mut journal = original.clone();
        let record = &mut journal["records"][&allocation];
        match field {
            "scope" => {
                record["metadata_retirement"]["plan"]["intent_sha256"] = "00".repeat(32).into()
            }
            "receipt" => {
                record["metadata_retirement"]["plan"]["metadata"]["receipt"]["cleanup_confirmed"] =
                    false.into()
            }
            _ => {
                record
                    .as_object_mut()
                    .unwrap()
                    .remove("metadata_retirement");
            }
        }
        fs::write(&path, serde_json::to_vec(&journal).unwrap()).unwrap();
        assert!(sandbox_supervisor::host::Host::open(next_config.clone()).is_err());
    }
    fs::write(&path, &bytes).unwrap();
    fs::create_dir(manifest.directory()).unwrap();
    assert!(sandbox_supervisor::host::Host::open(next_config.clone()).is_err());
    fs::remove_dir(manifest.directory()).unwrap();
    drop(sandbox_supervisor::host::Host::open(next_config).unwrap());
}
