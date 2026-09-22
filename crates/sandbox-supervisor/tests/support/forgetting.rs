//! Real host filesystem/RPC tests with synthetic controller metadata bindings.
//! The database tests independently verify issuance; these are not a worker loop.
use super::allocation_retirement::{permit, register, retirement};
use super::*;
use sandbox_protocol::{
    allocation_retirement::{DomainClosure, ForgetRequest, Request as RetirementRequest},
    supervisor::{AllocationForgetRequest, AllocationForgetState, AllocationMetadataRequest},
};
fn request(metadata: &RetirementRequest, epoch: i64) -> ForgetRequest {
    let mut claim = metadata.clone();
    claim.reporting_epoch = epoch;
    claim.revision = 1;
    claim.expires_unix_ms = guardian::wall_ms() + 30000;
    ForgetRequest {
        version: 1,
        claim,
        metadata_request: metadata.clone(),
    }
}
fn wire(r: &ForgetRequest) -> AllocationForgetRequest {
    AllocationForgetRequest {
        request_json: r.encode().unwrap(),
    }
}
async fn metadata(client: &mut SupervisorClient<Channel>, r: &RetirementRequest) {
    client
        .retire_allocation_metadata(AllocationMetadataRequest {
            request_json: r.encode().unwrap(),
        })
        .await
        .unwrap();
}
fn journal(f: &Fixture) -> serde_json::Value {
    serde_json::from_slice(&fs::read(f.config.state_root.join("host.json")).unwrap()).unwrap()
}
fn authority(f: &Fixture) -> sandbox_supervisor::launch_authority::AuthorityFile {
    sandbox_supervisor::launch_authority::AuthorityFile::open(
        f.config.state_root.join("a"),
        f.config.host,
        f.config.epoch,
        1,
    )
    .unwrap()
}
#[tokio::test]
#[ignore = "requires root, HUDSON_GUARDIAN_TEST_VM=1 and controlled KVM artifacts"]
async fn forgetting_requires_removed_metadata_and_recovers_after_the_record_is_gone() {
    let mut f = Fixture::configured_permits(true, false, true).await;
    let mut client = f.client().await;
    let create = f.request();
    let p = permit(&create);
    register(&mut client, &p).await;
    let m = retirement(p.clone());
    let r = request(&m, 1);
    assert!(client.forget_allocation(wire(&r)).await.is_err());
    metadata(&mut client, &m).await;
    for mode in 0..4 {
        let mut bad = r.clone();
        match mode {
            0 => bad.metadata_request.revision += 1,
            1 => {
                bad.claim.intent.release_evidence_sha256 = "cd".repeat(32);
                bad.metadata_request.intent = bad.claim.intent.clone();
            }
            2 => bad.claim.expires_unix_ms = 1,
            _ => bad.claim.reporting_epoch += 1,
        }
        assert!(
            client.forget_allocation(wire(&bad)).await.is_err(),
            "mode {mode}"
        );
        assert_eq!(journal(&f)["records"].as_object().unwrap().len(), 1);
    }
    for path in [
        f.config.state_root.join("a").join(p.allocation.to_string()),
        f.config.cgroup_parent.join(p.allocation.uuid().to_string()),
    ] {
        fs::create_dir(&path).unwrap();
        assert!(client.forget_allocation(wire(&r)).await.is_err());
        assert!(path.exists());
        fs::remove_dir(path).unwrap();
    }
    let expected = wire(&r);
    let done = client
        .forget_allocation(expected.clone())
        .await
        .unwrap()
        .into_inner();
    assert_eq!(done.request, Some(expected));
    assert_eq!(done.state, AllocationForgetState::Forgotten as i32);
    assert_eq!(journal(&f)["records"].as_object().unwrap().len(), 0);
    assert_eq!(
        client
            .forget_allocation(wire(&r))
            .await
            .unwrap()
            .get_ref()
            .state,
        AllocationForgetState::Retired as i32
    );
    assert!(
        client
            .retire_allocation_metadata(AllocationMetadataRequest {
                request_json: m.encode().unwrap()
            })
            .await
            .is_err()
    );
    assert!(
        client
            .inspect(InspectRequest {
                ownership: create.ownership.clone()
            })
            .await
            .is_err()
    );
    assert!(client.create(create).await.is_err());
    assert_eq!(journal(&f)["records"].as_object().unwrap().len(), 0);
    f.restart().await;
    client = f.client().await;
    assert!(client.forget_allocation(wire(&r)).await.is_err());
    assert_eq!(
        client
            .forget_allocation(wire(&request(&m, 2)))
            .await
            .unwrap()
            .get_ref()
            .state,
        AllocationForgetState::Retired as i32
    );
}
#[tokio::test]
#[ignore = "requires root, HUDSON_GUARDIAN_TEST_VM=1 and controlled KVM artifacts"]
async fn forgetting_restarts_at_each_durable_handoff_without_recreating_records() {
    for phase in 0..3 {
        let mut f = Fixture::configured_permits(true, false, true).await;
        let mut client = f.client().await;
        let p = permit(&f.request());
        register(&mut client, &p).await;
        let m = retirement(p.clone());
        metadata(&mut client, &m).await;
        f.restart_with(|f| {
            let a = authority(f);
            a.complete(1, &m.intent).unwrap();
            if phase >= 1 {
                let path = f.config.state_root.join("host.json");
                let mut j = journal(f);
                j["records"]
                    .as_object_mut()
                    .unwrap()
                    .remove(&p.allocation.to_string());
                fs::write(&path, serde_json::to_vec(&j).unwrap()).unwrap();
                fs::File::open(path).unwrap().sync_all().unwrap();
                fs::File::open(&f.config.state_root)
                    .unwrap()
                    .sync_all()
                    .unwrap();
            }
            if phase == 2 {
                a.forget(1, &m.intent).unwrap();
            }
        })
        .await;
        client = f.client().await;
        if phase < 2 {
            let mut bad = request(&m, 2);
            bad.claim.intent.release_evidence_sha256 = "cd".repeat(32);
            bad.metadata_request.intent = bad.claim.intent.clone();
            assert!(client.forget_allocation(wire(&bad)).await.is_err());
        }
        let state = client
            .forget_allocation(wire(&request(&m, 2)))
            .await
            .unwrap()
            .get_ref()
            .state;
        assert_eq!(
            state,
            if phase == 2 {
                AllocationForgetState::Retired
            } else {
                AllocationForgetState::Forgotten
            } as i32
        );
        assert_eq!(journal(&f)["records"].as_object().unwrap().len(), 0);
    }
}
#[tokio::test]
#[ignore = "requires root, HUDSON_GUARDIAN_TEST_VM=1 and controlled KVM artifacts"]
async fn incomplete_or_reappeared_state_cannot_be_adopted_as_completed_on_restart() {
    let mut f = Fixture::configured_permits(true, false, true).await;
    let mut client = f.client().await;
    let p = permit(&f.request());
    register(&mut client, &p).await;
    let m = retirement(p.clone());
    metadata(&mut client, &m).await;
    f.child.kill().unwrap();
    f.child.wait().unwrap();
    authority(&f).complete(1, &m.intent).unwrap();
    let path = f.config.state_root.join("host.json");
    let original = fs::read(&path).unwrap();
    let mut next = f.config.clone();
    next.epoch = 2;
    for mode in 0..3 {
        let mut j: serde_json::Value = serde_json::from_slice(&original).unwrap();
        let record = &mut j["records"][p.allocation.to_string()];
        match mode {
            0 => record["metadata_retirement"]["removed"] = false.into(),
            1 => record["metadata_retirement"]["plan"]["intent_sha256"] = "00".repeat(32).into(),
            _ => {
                record
                    .as_object_mut()
                    .unwrap()
                    .remove("metadata_retirement");
            }
        };
        fs::write(&path, serde_json::to_vec(&j).unwrap()).unwrap();
        assert!(sandbox_supervisor::host::Host::open(next.clone()).is_err());
    }
    fs::write(&path, &original).unwrap();
    let directory = f.config.state_root.join("a").join(p.allocation.to_string());
    fs::create_dir(&directory).unwrap();
    assert!(sandbox_supervisor::host::Host::open(next.clone()).is_err());
    fs::remove_dir(directory).unwrap();
    authority(&f).forget(1, &m.intent).unwrap();
    assert!(
        sandbox_supervisor::host::Host::open(next).is_err(),
        "a record must not survive root forgetting"
    );
}
#[tokio::test]
#[ignore = "requires root, HUDSON_GUARDIAN_TEST_VM=1 and controlled KVM artifacts"]
async fn real_execution_history_can_be_forgotten_and_new_ownership_admitted() {
    let f = Fixture::configured_permits(true, false, true).await;
    let mut client = f.client().await;
    let mut create = f.request();
    let p = permit(&create);
    register(&mut client, &p).await;
    create.launch_permit_json = serde_json::to_vec(&p).unwrap();
    let owner = create.ownership.clone().unwrap();
    let _ = client.create(create.clone()).await;
    f.ready(&mut client, &owner).await;
    let manifest = f.manifest(&owner);
    let guest = manifest.guest_client().unwrap();
    let (command, receipt) = super::history::execute(&mut client, &owner).await;
    client
        .retire_history(super::history::request(
            &owner,
            guest.context(),
            sandbox_protocol::history::Domain::Commands,
            receipt.operation_id,
            1,
        ))
        .await
        .unwrap();
    let mut stop = owner.clone();
    stop.operation_id = OperationId::generate().to_string();
    let _ = client
        .stop(StopRequest {
            ownership: Some(stop.clone()),
        })
        .await;
    f.released(&mut client, &stop).await;
    let mut m = retirement(p.clone());
    m.intent.commands = DomainClosure::Retired {
        through: receipt.operation_id,
    };
    metadata(&mut client, &m).await;
    client
        .forget_allocation(wire(&request(&m, 1)))
        .await
        .unwrap();
    assert!(!manifest.directory().exists());
    assert!(!manifest.group().exists());
    assert!(manifest.prepare().is_err());
    assert!(manifest.reconcile("delayed_forgetting").is_err());
    assert!(manifest.fence_unstarted().is_err());
    assert!(client.execute_command(command).await.is_err());
    assert!(client.create(create).await.is_err());
    let mut next = f.request();
    let mut p2 = permit(&next);
    p2.serial = 2;
    register(&mut client, &p2).await;
    next.launch_permit_json = serde_json::to_vec(&p2).unwrap();
    let owner = next.ownership.clone().unwrap();
    let _ = client.create(next).await;
    f.ready(&mut client, &owner).await;
    assert_eq!(journal(&f)["records"].as_object().unwrap().len(), 1);
    let _ = client
        .stop(StopRequest {
            ownership: Some(owner.clone()),
        })
        .await;
    f.released(&mut client, &owner).await;
}

#[tokio::test]
#[ignore = "requires root, HUDSON_GUARDIAN_TEST_VM=1 and controlled KVM artifacts"]
async fn failed_journal_removal_keeps_root_completion_and_requires_restart() {
    let mut f = Fixture::configured_permits(true, false, true).await;
    let mut client = f.client().await;
    let p = permit(&f.request());
    register(&mut client, &p).await;
    let m = retirement(p.clone());
    metadata(&mut client, &m).await;
    let r = request(&m, 1);
    let next = f.config.state_root.join("a/launch.next");
    fs::create_dir(&next).unwrap();
    assert!(client.forget_allocation(wire(&r)).await.is_err());
    assert_eq!(journal(&f)["records"].as_object().unwrap().len(), 1);
    assert!(authority(&f).completed(1, &m.intent).is_err());
    fs::remove_dir(next).unwrap();
    let path = f.config.state_root.join("host.json");
    let backup = f.config.state_root.join("saved-host.json");
    fs::rename(&path, &backup).unwrap();
    fs::create_dir(&path).unwrap();
    assert!(client.forget_allocation(wire(&r)).await.is_err());
    drop(authority(&f).completed(1, &m.intent).unwrap());
    assert!(f.config.state_root.join("journal.next").is_file());
    assert!(
        client.forget_allocation(wire(&r)).await.is_err(),
        "uncertain journal must be poisoned"
    );
    f.restart_with(|_| {
        fs::remove_dir(&path).unwrap();
        fs::rename(&backup, &path).unwrap();
    })
    .await;
    client = f.client().await;
    assert_eq!(
        client
            .forget_allocation(wire(&request(&m, 2)))
            .await
            .unwrap()
            .get_ref()
            .state,
        AllocationForgetState::Forgotten as i32
    );
    assert_eq!(journal(&f)["records"].as_object().unwrap().len(), 0);
}

#[tokio::test]
#[ignore = "requires root, HUDSON_GUARDIAN_TEST_VM=1 and controlled KVM artifacts"]
async fn journal_staging_is_bounded_and_never_adopted_as_authoritative_state() {
    use std::os::unix::fs::{OpenOptionsExt, symlink};
    let mut f = Fixture::configured_permits(true, false, true).await;
    for _ in 0..3 {
        f.restart_with(|f| {
            let p = f.config.state_root.join("journal.next");
            let mut file = fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .mode(0o600)
                .open(p)
                .unwrap();
            std::io::Write::write_all(&mut file, b"partial uncommitted journal").unwrap();
            file.sync_all().unwrap();
        })
        .await;
        assert!(!f.config.state_root.join("journal.next").exists());
        assert_eq!(journal(&f)["records"].as_object().unwrap().len(), 0);
    }
    f.child.kill().unwrap();
    f.child.wait().unwrap();
    let path = f.config.state_root.join("host.json");
    let original = fs::read(&path).unwrap();
    let stage = f.config.state_root.join("journal.next");
    let mut next = f.config.clone();
    next.epoch += 1;
    symlink(&path, &stage).unwrap();
    assert!(sandbox_supervisor::host::Host::open(next.clone()).is_err());
    fs::remove_file(&stage).unwrap();
    fs::hard_link(&path, &stage).unwrap();
    assert!(sandbox_supervisor::host::Host::open(next.clone()).is_err());
    fs::remove_file(&stage).unwrap();
    assert_eq!(fs::read(&path).unwrap(), original);
    fs::rename(&path, &stage).unwrap();
    assert!(sandbox_supervisor::host::Host::open(next).is_err());
    assert!(!path.exists(), "missing main must not adopt staging");
    fs::rename(stage, path).unwrap();
}

#[tokio::test]
#[ignore = "requires root, HUDSON_GUARDIAN_TEST_VM=1 and controlled KVM artifacts"]
async fn registered_fixture_releases_test_images_after_confirmed_forgetting() {
    let f = Fixture::configured_permits(true, false, true).await;
    let mut client = f.client().await;
    let p = permit(&f.request());
    register(&mut client, &p).await;
    let m = retirement(p);
    metadata(&mut client, &m).await;
    client
        .forget_allocation(wire(&request(&m, 1)))
        .await
        .unwrap();
    let path = f.vm.temp.path().to_path_buf();
    drop(client);
    drop(f);
    assert!(
        !path.exists(),
        "confirmed fixture cleanup must release test-only images"
    );
}
