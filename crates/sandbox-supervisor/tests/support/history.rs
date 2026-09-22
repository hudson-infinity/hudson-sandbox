use super::*;
use sandbox_protocol::{
    guest_model as gm,
    history::{Barrier, Domain},
    supervisor::{CommandInspection, CommandRequest, HistoryRequest},
};
fn request(
    owner: &Ownership,
    context: &gm::Context,
    domain: Domain,
    through: OperationId,
    revision: i64,
) -> HistoryRequest {
    HistoryRequest {
        ownership: Some(lease(owner, revision)),
        barrier: Some(
            (&Barrier {
                version: 1,
                context: context.clone(),
                domain,
                through,
            })
                .into(),
        ),
    }
}
async fn execute(
    c: &mut SupervisorClient<Channel>,
    o: &Ownership,
) -> (CommandRequest, gm::Receipt) {
    let id = OperationId::generate();
    let mut owner = o.clone();
    owner.operation_id = id.to_string();
    let command = gm::Execute {
        operation_id: id,
        argv: vec!["/bin/busybox".into(), "true".into()],
        env: BTreeMap::new(),
        cwd: "/".into(),
        deadline_unix_ms: guardian::wall_ms() + 60000,
        output_limit: 1024,
    };
    let request = CommandRequest {
        ownership: Some(owner.clone()),
        command: Some((&command).into()),
    };
    c.execute_command(request.clone()).await.unwrap();
    let until = Instant::now() + Duration::from_secs(5);
    loop {
        let observed = c
            .inspect_command(CommandInspection {
                ownership: Some(owner.clone()),
                command_digest: command.digest().unwrap().to_vec(),
            })
            .await
            .unwrap()
            .into_inner();
        if let Some(receipt) = observed.receipt {
            let receipt: gm::Receipt = receipt.try_into().unwrap();
            if receipt.state.terminal() {
                assert!(receipt.cleanup_confirmed);
                return (request, receipt);
            }
        }
        assert!(Instant::now() < until, "command never finished");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}
#[tokio::test]
#[ignore = "requires root, HUDSON_GUARDIAN_TEST_VM=1 and aarch64 KVM artifacts"]
async fn real_history_rpc_reclaims_full_command_and_file_slots_and_preserves_fences() {
    let mut f = Fixture::new().await;
    let mut c = f.client().await;
    let create = f.request();
    let owner = create.ownership.clone().unwrap();
    let _ = c.create(create).await;
    f.ready(&mut c, &owner).await;
    let guest = f.manifest(&owner).guest_client().unwrap();
    let mut commands = Vec::new();
    for _ in 0..32 {
        commands.push(execute(&mut c, &owner).await);
    }
    let through = commands.iter().map(|(_, r)| r.operation_id).max().unwrap();
    let retire = request(&owner, guest.context(), Domain::Commands, through, 1);
    let mut reader = transport::connect(
        &f.url,
        f.config.host,
        f.tls.ca.pem().as_bytes(),
        f.reader.cert.pem().as_bytes(),
        f.reader.key.serialize_pem().as_bytes(),
    )
    .await
    .unwrap();
    assert_eq!(
        reader
            .retire_history(retire.clone())
            .await
            .unwrap_err()
            .code(),
        Code::PermissionDenied
    );
    let mut too_many = commands[0].0.clone();
    let extra = OperationId::generate();
    too_many.ownership.as_mut().unwrap().operation_id = extra.to_string();
    too_many.command.as_mut().unwrap().operation_id = extra.to_string();
    assert_eq!(
        c.execute_command(too_many).await.unwrap_err().code(),
        Code::ResourceExhausted
    );
    let mut wrong = retire.clone();
    wrong
        .barrier
        .as_mut()
        .unwrap()
        .context
        .as_mut()
        .unwrap()
        .boot_id = "wrong".into();
    assert_eq!(
        c.retire_history(wrong).await.unwrap_err().code(),
        Code::FailedPrecondition
    );
    let completed = c.retire_history(retire.clone()).await.unwrap().into_inner();
    assert!(!completed.simulated);
    assert_eq!(completed.completed, retire.barrier);
    assert_eq!(
        c.retire_history(retire.clone())
            .await
            .unwrap()
            .get_ref()
            .completed,
        retire.barrier
    );
    assert!(guest.inspect(commands[0].1.operation_id).await.is_err());
    for (command, receipt) in &commands {
        assert_eq!(
            c.execute_command(command.clone()).await.unwrap_err().code(),
            Code::FailedPrecondition
        );
        let inspect = CommandInspection {
            ownership: command.ownership.clone(),
            command_digest: receipt.digest.to_vec(),
        };
        assert_eq!(
            c.inspect_command(inspect.clone()).await.unwrap_err().code(),
            Code::FailedPrecondition
        );
        assert_eq!(
            c.cancel_command(inspect).await.unwrap_err().code(),
            Code::FailedPrecondition
        );
    }
    execute(&mut c, &owner).await;
    let mut uploads = Vec::new();
    for _ in 0..16 {
        let upload = super::supervisor_files::upload(&owner, "retirement-empty", b"");
        c.begin_file(upload.clone()).await.unwrap();
        c.abort_file(upload.clone()).await.unwrap();
        uploads.push(upload);
    }
    let extra = super::supervisor_files::upload(&owner, "retirement-new", b"");
    assert_eq!(
        c.begin_file(extra.clone()).await.unwrap_err().code(),
        Code::ResourceExhausted
    );
    let through = uploads
        .iter()
        .map(|r| {
            r.upload
                .as_ref()
                .unwrap()
                .operation_id
                .parse::<OperationId>()
                .unwrap()
        })
        .max()
        .unwrap();
    let file_retire = request(&owner, guest.context(), Domain::Files, through, 1);
    c.retire_history(file_retire.clone()).await.unwrap();
    for old in uploads {
        assert_eq!(
            c.begin_file(old.clone()).await.unwrap_err().code(),
            Code::FailedPrecondition
        );
        assert_eq!(
            c.commit_file(old.clone()).await.unwrap_err().code(),
            Code::FailedPrecondition
        );
        assert_eq!(
            c.abort_file(old.clone()).await.unwrap_err().code(),
            Code::FailedPrecondition
        );
        assert_eq!(
            c.inspect_file(old).await.unwrap_err().code(),
            Code::FailedPrecondition
        );
    }
    c.begin_file(extra.clone()).await.unwrap();
    c.abort_file(extra).await.unwrap();
    // Aborted transfers retain their declared budgets even without uploading bytes.
    let mut byte_history = Vec::new();
    for _ in 0..8 {
        let mut upload = super::supervisor_files::upload(&owner, "byte-budget", b"");
        upload.upload.as_mut().unwrap().size = sandbox_protocol::files::MAX_FILE_BYTES;
        c.begin_file(upload.clone()).await.unwrap();
        c.abort_file(upload.clone()).await.unwrap();
        byte_history.push(upload);
    }
    let one_byte = super::supervisor_files::upload(&owner, "one-byte", b"x");
    assert_eq!(
        c.begin_file(one_byte.clone()).await.unwrap_err().code(),
        Code::ResourceExhausted
    );
    let through = byte_history
        .iter()
        .map(|r| {
            r.upload
                .as_ref()
                .unwrap()
                .operation_id
                .parse::<OperationId>()
                .unwrap()
        })
        .max()
        .unwrap();
    c.retire_history(request(&owner, guest.context(), Domain::Files, through, 2))
        .await
        .unwrap();
    c.begin_file(one_byte.clone()).await.unwrap();
    c.abort_file(one_byte).await.unwrap();
    let journal: serde_json::Value =
        serde_json::from_slice(&fs::read(f.config.state_root.join("host.json")).unwrap()).unwrap();
    let record = &journal["records"][&owner.allocation_id];
    assert_eq!(record["commands"].as_object().unwrap().len(), 1);
    assert_eq!(record["files"].as_object().unwrap().len(), 1);
    assert_eq!(record["command_history"]["completed"], true);
    assert_eq!(record["file_history"]["completed"], true);
    // Stop acknowledgement can be lost while cleanup progresses. Require verified
    // release below rather than treating the initial RPC result as completion.
    let _ = c
        .stop(StopRequest {
            ownership: Some(owner.clone()),
        })
        .await;
    f.released(&mut c, &owner).await;
    assert_eq!(
        c.retire_history(retire.clone())
            .await
            .unwrap()
            .get_ref()
            .completed,
        retire.barrier
    );
    f.restart().await;
    let journal: serde_json::Value =
        serde_json::from_slice(&fs::read(f.config.state_root.join("host.json")).unwrap()).unwrap();
    assert_eq!(
        journal["records"][&owner.allocation_id]["command_history"],
        record["command_history"]
    );
    assert_eq!(
        f.client()
            .await
            .retire_history(retire)
            .await
            .unwrap_err()
            .code(),
        Code::FailedPrecondition
    );
}
#[tokio::test]
#[ignore = "requires root, HUDSON_GUARDIAN_TEST_VM=1 and aarch64 KVM artifacts"]
async fn real_history_reconciles_guest_completion_but_refuses_enlarged_guest_floor() {
    let mut f = Fixture::new().await;
    let mut c = f.client().await;
    let create = f.request();
    let owner = create.ownership.clone().unwrap();
    let _ = c.create(create).await;
    f.ready(&mut c, &owner).await;
    let guest = f.manifest(&owner).guest_client().unwrap();
    let (old, receipt) = execute(&mut c, &owner).await;
    let retire = request(
        &owner,
        guest.context(),
        Domain::Commands,
        receipt.operation_id,
        1,
    );
    let barrier: Barrier = retire.barrier.clone().unwrap().try_into().unwrap();
    // Model a guest completion whose acknowledgement was lost before host completion.
    guest.retire_history(&barrier).await.unwrap();
    c.retire_history(retire).await.unwrap();
    assert!(c.execute_command(old).await.is_err());
    let lower_id: OperationId = "op_019a9fad-3000-7000-8000-000000000001".parse().unwrap();
    assert!(lower_id < barrier.through);
    let lower = request(&owner, guest.context(), Domain::Commands, lower_id, 2);
    for _ in 0..2 {
        assert_eq!(
            c.retire_history(lower.clone())
                .await
                .unwrap()
                .get_ref()
                .completed,
            Some((&barrier).into())
        );
    }
    let changed = request(
        &owner,
        guest.context(),
        Domain::Commands,
        barrier.through,
        2,
    );
    assert_eq!(
        c.retire_history(changed).await.unwrap_err().code(),
        Code::FailedPrecondition
    );
    let (old, receipt) = execute(&mut c, &owner).await;
    let retire = request(
        &owner,
        guest.context(),
        Domain::Commands,
        receipt.operation_id,
        3,
    );
    let mut larger: Barrier = retire.barrier.clone().unwrap().try_into().unwrap();
    larger.through = OperationId::generate();
    assert!(larger.through > receipt.operation_id);
    guest.retire_history(&larger).await.unwrap();
    assert_eq!(
        c.retire_history(retire.clone()).await.unwrap_err().code(),
        Code::Unavailable
    );
    let journal: serde_json::Value =
        serde_json::from_slice(&fs::read(f.config.state_root.join("host.json")).unwrap()).unwrap();
    let record = &journal["records"][&owner.allocation_id];
    assert_eq!(record["command_history"]["completed"], false);
    assert!(
        record["commands"]
            .get(receipt.operation_id.to_string())
            .is_some()
    );
    assert_eq!(
        c.execute_command(old).await.unwrap_err().code(),
        Code::FailedPrecondition
    );
    assert_eq!(
        c.retire_history(retire).await.unwrap_err().code(),
        Code::Unavailable
    );
    f.child.kill().unwrap();
    f.child.wait().unwrap();
    let path = f.config.state_root.join("host.json");
    let original = fs::read(&path).unwrap();
    let mut corrupt: serde_json::Value = serde_json::from_slice(&original).unwrap();
    corrupt["records"][&owner.allocation_id]["command_history"]["completed"] = true.into();
    fs::write(&path, serde_json::to_vec(&corrupt).unwrap()).unwrap();
    let mut next = f.config.clone();
    next.epoch += 1;
    assert!(
        sandbox_supervisor::host::Host::open(next.clone()).is_err(),
        "completion with retained covered records must fail closed"
    );
    fs::write(&path, original).unwrap();
    let recovered = sandbox_supervisor::host::Host::open(next).unwrap();
    drop(recovered);
}

#[tokio::test]
#[ignore = "requires root, HUDSON_GUARDIAN_TEST_VM=1 and aarch64 KVM artifacts"]
async fn real_history_refuses_staging_and_failed_host_persistence_never_deletes_guest_history() {
    let mut f = Fixture::new().await;
    let mut c = f.client().await;
    let create = f.request();
    let owner = create.ownership.clone().unwrap();
    let _ = c.create(create).await;
    f.ready(&mut c, &owner).await;
    let guest = f.manifest(&owner).guest_client().unwrap();
    let upload = super::supervisor_files::upload(&owner, "staging-retirement", b"");
    c.begin_file(upload.clone()).await.unwrap();
    let id = upload
        .upload
        .as_ref()
        .unwrap()
        .operation_id
        .parse()
        .unwrap();
    let retire_file = request(&owner, guest.context(), Domain::Files, id, 1);
    assert_eq!(
        c.retire_history(retire_file.clone())
            .await
            .unwrap_err()
            .code(),
        Code::FailedPrecondition
    );
    let path = f.config.state_root.join("host.json");
    let state: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert!(
        state["records"][&owner.allocation_id]
            .get("file_history")
            .is_none()
    );
    c.abort_file(upload).await.unwrap();
    c.retire_history(retire_file).await.unwrap();
    let (command, receipt) = execute(&mut c, &owner).await;
    let retire = request(
        &owner,
        guest.context(),
        Domain::Commands,
        receipt.operation_id,
        1,
    );
    let before = fs::read(&path).unwrap();
    let backup = f.config.state_root.join("before-history.json");
    fs::rename(&path, &backup).unwrap();
    fs::create_dir(&path).unwrap();
    assert_eq!(
        c.retire_history(retire).await.unwrap_err().code(),
        Code::Unavailable
    );
    assert!(
        guest.inspect(receipt.operation_id).await.is_ok(),
        "guest must not be contacted after failed host persistence"
    );
    assert_eq!(
        c.execute_command(command).await.unwrap_err().code(),
        Code::Unavailable
    );
    assert_eq!(fs::read(&backup).unwrap(), before);
    fs::remove_dir(&path).unwrap();
    fs::rename(backup, &path).unwrap();
    f.restart().await;
}
