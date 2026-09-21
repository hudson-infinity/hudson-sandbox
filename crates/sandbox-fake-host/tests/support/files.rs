use super::*;
use sandbox_protocol::{
    files as m,
    supervisor::{CommandRequest, FileObservation, FileRequest, FileWriteRequest},
    supervisor_files::MAX_FILES,
};
use sha2::{Digest, Sha256};
fn request(owner: &Ownership, data: &[u8]) -> FileRequest {
    let mut owner = owner.clone();
    owner.operation_id = OperationId::generate().to_string();
    FileRequest {
        upload: Some(
            (&m::Upload {
                operation_id: owner.operation_id.parse().unwrap(),
                path: "file".into(),
                size: data.len() as u64,
                sha256: Sha256::digest(data).into(),
                mode: 0o644,
            })
                .into(),
        ),
        ownership: Some(owner),
    }
}
async fn begin(fake: &FakeHost, r: &FileRequest) -> FileObservation {
    fake.begin_file(Request::new(r.clone()))
        .await
        .unwrap()
        .into_inner()
}
async fn write(
    fake: &FakeHost,
    r: &FileRequest,
    offset: u64,
    data: &[u8],
) -> Result<tonic::Response<FileObservation>, tonic::Status> {
    fake.write_file(Request::new(FileWriteRequest {
        request: Some(r.clone()),
        offset,
        data: data.to_vec(),
    }))
    .await
}
#[tokio::test]
async fn file_retries_preserve_one_commit_and_conflicting_chunks_or_descriptors_fail() {
    let (fake, create) = fixture();
    fake.create(Request::new(create.clone())).await.unwrap();
    let r = request(create.ownership.as_ref().unwrap(), b"data");
    fake.lose_next_file_reply().await;
    assert!(fake.begin_file(Request::new(r.clone())).await.is_err());
    assert_eq!(begin(&fake, &r).await.state, 1);
    assert_eq!(
        write(&fake, &r, 0, b"da").await.unwrap().get_ref().stored,
        Some(2)
    );
    assert_eq!(
        write(&fake, &r, 0, b"da").await.unwrap().get_ref().stored,
        Some(2)
    );
    assert_eq!(
        write(&fake, &r, 0, b"zz").await.unwrap_err().code(),
        Code::AlreadyExists
    );
    write(&fake, &r, 2, b"ta").await.unwrap();
    fake.lose_next_file_reply().await;
    assert!(fake.commit_file(Request::new(r.clone())).await.is_err());
    assert_eq!(
        fake.inspect_file(Request::new(r.clone()))
            .await
            .unwrap()
            .get_ref()
            .state,
        3
    );
    assert_eq!(
        fake.commit_file(Request::new(r.clone()))
            .await
            .unwrap()
            .get_ref()
            .state,
        3
    );
    assert_eq!(fake.total_file_commits().await, 1);
    assert_eq!(
        write(&fake, &r, 0, b"data").await.unwrap().get_ref().stored,
        None
    );
    let mut changed = r.clone();
    changed.upload.as_mut().unwrap().path = "other".into();
    assert_eq!(
        fake.begin_file(Request::new(changed))
            .await
            .unwrap_err()
            .code(),
        Code::AlreadyExists
    );
    let mut stale = r.clone();
    stale.ownership.as_mut().unwrap().claim_revision = 2;
    begin(&fake, &stale).await;
    assert_eq!(
        fake.inspect_file(Request::new(r)).await.unwrap_err().code(),
        Code::FailedPrecondition
    );
}
#[tokio::test]
async fn file_absence_inspection_blocks_late_begin_and_mismatched_ownership() {
    let (fake, create) = fixture();
    fake.create(Request::new(create.clone())).await.unwrap();
    let mut r = request(create.ownership.as_ref().unwrap(), b"data");
    assert!(
        fake.inspect_file(Request::new(r.clone()))
            .await
            .unwrap()
            .get_ref()
            .not_started
    );
    r.ownership.as_mut().unwrap().claim_revision += 1;
    assert!(begin(&fake, &r).await.not_started);
    for mode in 0..4 {
        let mut bad = r.clone();
        let o = bad.ownership.as_mut().unwrap();
        match mode {
            0 => o.project_id = ProjectId::generate().to_string(),
            1 => o.generation += 1,
            2 => o.supervisor_epoch += 1,
            _ => o.claim_expires_unix_ms = unix_ms().unwrap() - 1,
        };
        assert_eq!(
            fake.inspect_file(Request::new(bad))
                .await
                .unwrap_err()
                .code(),
            Code::FailedPrecondition
        );
    }
    assert_eq!(fake.total_file_commits().await, 0);
}
#[tokio::test]
async fn file_stop_and_abort_do_not_turn_unknown_publication_into_success() {
    let (fake, create) = fixture();
    fake.create(Request::new(create.clone())).await.unwrap();
    let r = request(create.ownership.as_ref().unwrap(), b"good");
    begin(&fake, &r).await;
    write(&fake, &r, 0, b"bad!").await.unwrap();
    let uncertain = fake.commit_file(Request::new(r.clone())).await.unwrap();
    assert_eq!(uncertain.get_ref().state, 0);
    assert_eq!(
        write(&fake, &r, 0, b"good").await.unwrap().get_ref().stored,
        None
    );
    assert_eq!(
        fake.abort_file(Request::new(r.clone()))
            .await
            .unwrap()
            .get_ref()
            .state,
        5
    );
    assert_eq!(
        fake.commit_file(Request::new(r))
            .await
            .unwrap()
            .get_ref()
            .state,
        5
    );
    let pending = request(create.ownership.as_ref().unwrap(), b"later");
    begin(&fake, &pending).await;
    fake.stop(Request::new(StopRequest {
        ownership: create.ownership,
    }))
    .await
    .unwrap();
    let stopped = fake.commit_file(Request::new(pending)).await.unwrap();
    assert_eq!(stopped.get_ref().state, 0);
    assert!(!stopped.get_ref().not_started);
    assert_eq!(fake.total_file_commits().await, 0);
}
#[tokio::test]
async fn full_command_and_file_budgets_leave_space_for_destroy() {
    let (fake, create) = fixture();
    fake.create(Request::new(create.clone())).await.unwrap();
    let owner = create.ownership.as_ref().unwrap();
    for _ in 0..sandbox_protocol::command::MAX_COMMANDS {
        let mut o = owner.clone();
        o.operation_id = OperationId::generate().to_string();
        let command = sandbox_protocol::guest_model::Execute {
            operation_id: o.operation_id.parse().unwrap(),
            argv: vec!["true".into()],
            env: Default::default(),
            cwd: "/".into(),
            deadline_unix_ms: unix_ms().unwrap() + 20000,
            output_limit: 1,
        };
        fake.execute_command(Request::new(CommandRequest {
            ownership: Some(o),
            command: Some((&command).into()),
        }))
        .await
        .unwrap();
    }
    for _ in 0..MAX_FILES {
        let r = request(owner, b"");
        begin(&fake, &r).await;
        fake.commit_file(Request::new(r)).await.unwrap();
    }
    assert_eq!(
        fake.begin_file(Request::new(request(owner, b"")))
            .await
            .unwrap_err()
            .code(),
        Code::ResourceExhausted
    );
    let mut destroy = owner.clone();
    destroy.operation_id = OperationId::generate().to_string();
    assert_eq!(
        fake.stop(Request::new(StopRequest {
            ownership: Some(destroy)
        }))
        .await
        .unwrap()
        .get_ref()
        .state,
        AllocationState::Released as i32
    );
}
#[tokio::test]
async fn file_and_command_operation_ids_cannot_share_a_fence() {
    let (fake, create) = fixture();
    fake.create(Request::new(create.clone())).await.unwrap();
    let r = request(create.ownership.as_ref().unwrap(), b"");
    begin(&fake, &r).await;
    let command = sandbox_protocol::guest_model::Execute {
        operation_id: r.ownership.as_ref().unwrap().operation_id.parse().unwrap(),
        argv: vec!["true".into()],
        env: Default::default(),
        cwd: "/".into(),
        deadline_unix_ms: unix_ms().unwrap() + 20000,
        output_limit: 1,
    };
    assert_eq!(
        fake.execute_command(Request::new(CommandRequest {
            ownership: r.ownership,
            command: Some((&command).into())
        }))
        .await
        .unwrap_err()
        .code(),
        Code::AlreadyExists
    );
    let reused = FileRequest {
        ownership: create.ownership.clone(),
        upload: Some(
            (&m::Upload {
                operation_id: create.ownership.unwrap().operation_id.parse().unwrap(),
                path: "file".into(),
                size: 0,
                sha256: Sha256::digest(b"").into(),
                mode: 0o644,
            })
                .into(),
        ),
    };
    assert_eq!(
        fake.begin_file(Request::new(reused))
            .await
            .unwrap_err()
            .code(),
        Code::AlreadyExists
    );
}
#[tokio::test]
async fn fake_staging_is_globally_bounded_and_stop_releases_its_memory() {
    let (fake, create) = fixture();
    fake.create(Request::new(create.clone())).await.unwrap();
    let chunk = vec![1; m::MAX_CHUNK_BYTES];
    for _ in 0..8 {
        let mut r = request(create.ownership.as_ref().unwrap(), b"");
        r.upload.as_mut().unwrap().size = m::MAX_FILE_BYTES;
        begin(&fake, &r).await;
        for offset in (0..m::MAX_FILE_BYTES).step_by(m::MAX_CHUNK_BYTES) {
            write(&fake, &r, offset, &chunk).await.unwrap();
        }
    }
    let mut second = create.clone();
    let o = second.ownership.as_mut().unwrap();
    o.allocation_id = AllocationId::generate().to_string();
    o.sandbox_id = SandboxId::generate().to_string();
    o.operation_id = OperationId::generate().to_string();
    fake.create(Request::new(second.clone())).await.unwrap();
    let r = request(second.ownership.as_ref().unwrap(), &chunk);
    begin(&fake, &r).await;
    assert_eq!(
        write(&fake, &r, 0, &chunk).await.unwrap_err().code(),
        Code::ResourceExhausted
    );
    fake.stop(Request::new(StopRequest {
        ownership: create.ownership,
    }))
    .await
    .unwrap();
    assert_eq!(
        write(&fake, &r, 0, &chunk).await.unwrap().get_ref().stored,
        Some(chunk.len() as u64)
    );
}
