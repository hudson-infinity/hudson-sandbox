//! State-machine evidence only. No process or VM is started by these tests.
#![allow(clippy::unwrap_used, clippy::expect_used)]
use sandbox_fake_host::{FakeConfig, FakeHost, unix_ms};
use sandbox_protocol::{
    AllocationId, HostId, Id, OperationId, ProjectId, SandboxId,
    supervisor::{
        AllocationState, CreateRequest, InspectRequest, Ownership, Resources, StopRequest,
        supervisor_server::Supervisor,
    },
};
use std::collections::BTreeSet;
use tonic::{Code, Request};

fn fixture() -> (FakeHost, CreateRequest) {
    let host = HostId::generate();
    let digest = format!("sha256:{}", "a".repeat(64));
    let fake = FakeHost::new(FakeConfig {
        host,
        epoch: 1,
        images: BTreeSet::from([digest.clone()]),
        capacity: Resources {
            vcpu: 4,
            memory_mib: 8192,
            disk_mib: 65536,
        },
    })
    .unwrap();
    let request = CreateRequest {
        ownership: Some(Ownership {
            host_id: host.to_string(),
            project_id: ProjectId::generate().to_string(),
            sandbox_id: SandboxId::generate().to_string(),
            allocation_id: AllocationId::generate().to_string(),
            operation_id: OperationId::generate().to_string(),
            generation: 1,
            supervisor_epoch: 1,
            claim_revision: 1,
            claim_expires_unix_ms: unix_ms().unwrap() + 30_000,
        }),
        image_digest: digest,
        resources: Some(Resources {
            vcpu: 2,
            memory_mib: 2048,
            disk_mib: 8192,
        }),
        allocation_expires_unix_ms: unix_ms().unwrap() + 30_000,
    };
    (fake, request)
}

#[tokio::test]
async fn concurrent_duplicate_creates_have_one_start() {
    let (fake, request) = fixture();
    let mut tasks = tokio::task::JoinSet::new();
    for _ in 0..8 {
        let (fake, request) = (fake.clone(), request.clone());
        tasks.spawn(async move {
            fake.create(Request::new(request))
                .await
                .unwrap()
                .into_inner()
        });
    }
    while let Some(result) = tasks.join_next().await {
        let observation = result.unwrap();
        assert!(observation.simulated);
        assert_eq!(observation.state, AllocationState::Ready as i32);
        assert_eq!(observation.start_count, 1);
    }
    assert_eq!(fake.total_starts().await, 1);
    let mut changed = request.clone();
    changed.resources.as_mut().unwrap().vcpu = 1;
    assert_eq!(
        fake.create(Request::new(changed)).await.unwrap_err().code(),
        Code::AlreadyExists
    );
}

#[tokio::test]
async fn lost_acknowledgement_reconciles_without_replay() {
    let (fake, request) = fixture();
    fake.lose_next_create_reply().await;
    assert_eq!(
        fake.create(Request::new(request.clone()))
            .await
            .unwrap_err()
            .code(),
        Code::Unavailable
    );
    let mut current = request.ownership.clone().unwrap();
    current.claim_revision = 2;
    let observation = fake
        .inspect(Request::new(InspectRequest {
            ownership: Some(current),
        }))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(observation.start_count, 1);
    assert_eq!(fake.total_starts().await, 1);
    assert_eq!(observation.state, AllocationState::Ready as i32);
    assert_eq!(
        fake.create(Request::new(request)).await.unwrap_err().code(),
        Code::FailedPrecondition
    );
}

#[tokio::test]
async fn inspecting_absence_fences_a_delayed_stale_create() {
    let (fake, request) = fixture();
    let mut current = request.ownership.clone().unwrap();
    current.claim_revision = 2;
    let observation = fake
        .inspect(Request::new(InspectRequest {
            ownership: Some(current),
        }))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(observation.state, AllocationState::Absent as i32);
    assert_eq!(observation.start_count, 0);
    assert_eq!(
        fake.create(Request::new(request)).await.unwrap_err().code(),
        Code::FailedPrecondition
    );
}

#[tokio::test]
async fn stop_before_create_blocks_even_an_unseen_operation() {
    let (fake, request) = fixture();
    let mut owner = request.ownership.clone().unwrap();
    owner.operation_id = OperationId::generate().to_string();
    let observation = fake
        .stop(Request::new(StopRequest {
            ownership: Some(owner),
        }))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(observation.state, AllocationState::Absent as i32);
    assert_eq!(
        fake.create(Request::new(request)).await.unwrap_err().code(),
        Code::FailedPrecondition
    );
}

#[tokio::test]
async fn stop_is_idempotent_and_old_create_never_restarts() {
    let (fake, request) = fixture();
    fake.create(Request::new(request.clone())).await.unwrap();
    let mut stop = request.ownership.clone().unwrap();
    stop.operation_id = OperationId::generate().to_string();
    for _ in 0..2 {
        let observation = fake
            .stop(Request::new(StopRequest {
                ownership: Some(stop.clone()),
            }))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(observation.state, AllocationState::Released as i32);
    }
    let observation = fake
        .create(Request::new(request))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(observation.start_count, 1);
    assert_eq!(observation.state, AllocationState::Released as i32);
}

#[tokio::test]
async fn epoch_generation_and_project_must_match() {
    let (fake, request) = fixture();
    fake.create(Request::new(request.clone())).await.unwrap();
    for field in ["epoch", "generation", "project", "sandbox", "host"] {
        let mut owner = request.ownership.clone().unwrap();
        match field {
            "epoch" => owner.supervisor_epoch += 1,
            "generation" => owner.generation += 1,
            "project" => owner.project_id = ProjectId::generate().to_string(),
            "sandbox" => owner.sandbox_id = SandboxId::generate().to_string(),
            _ => owner.host_id = HostId::generate().to_string(),
        }
        assert_eq!(
            fake.inspect(Request::new(InspectRequest {
                ownership: Some(owner)
            }))
            .await
            .unwrap_err()
            .code(),
            Code::FailedPrecondition
        );
    }
}

#[tokio::test]
async fn limits_allowlist_and_deadlines_fail_closed() {
    for scenario in [
        "image",
        "cpu",
        "memory",
        "disk",
        "expired",
        "unbounded",
        "lease",
        "missing",
        "id",
    ] {
        let (fake, mut request) = fixture();
        match scenario {
            "image" => request.image_digest = format!("sha256:{}", "b".repeat(64)),
            "cpu" => request.resources.as_mut().unwrap().vcpu = 5,
            "memory" => request.resources.as_mut().unwrap().memory_mib = 8193,
            "disk" => request.resources.as_mut().unwrap().disk_mib = 65537,
            "expired" => request.ownership.as_mut().unwrap().claim_expires_unix_ms = 1,
            "unbounded" => request.ownership.as_mut().unwrap().claim_expires_unix_ms = i64::MAX,
            "lease" => request.allocation_expires_unix_ms = i64::MAX,
            "missing" => request.ownership = None,
            _ => request.ownership.as_mut().unwrap().allocation_id = "not-an-id".into(),
        }
        assert!(
            fake.create(Request::new(request)).await.is_err(),
            "{scenario}"
        );
    }
}

#[tokio::test]
async fn capacity_and_replacement_require_release() {
    let (fake, request) = fixture();
    fake.create(Request::new(request.clone())).await.unwrap();
    let mut replacement = request.clone();
    let owner = replacement.ownership.as_mut().unwrap();
    owner.allocation_id = AllocationId::generate().to_string();
    owner.generation = 2;
    owner.operation_id = OperationId::generate().to_string();
    assert_eq!(
        fake.create(Request::new(replacement.clone()))
            .await
            .unwrap_err()
            .code(),
        Code::FailedPrecondition
    );
    let mut other = replacement.clone();
    other.ownership.as_mut().unwrap().allocation_id = AllocationId::generate().to_string();
    other.ownership.as_mut().unwrap().sandbox_id = SandboxId::generate().to_string();
    other.resources.as_mut().unwrap().vcpu = 3;
    assert_eq!(
        fake.create(Request::new(other)).await.unwrap_err().code(),
        Code::ResourceExhausted
    );
    fake.stop(Request::new(StopRequest {
        ownership: request.ownership,
    }))
    .await
    .unwrap();
    assert_eq!(
        fake.create(Request::new(replacement))
            .await
            .unwrap()
            .into_inner()
            .state,
        AllocationState::Ready as i32
    );
}

#[tokio::test(start_paused = true)]
async fn watchdog_expires_without_traffic_and_retry_cannot_extend_it() {
    let (fake, request) = fixture();
    fake.create(Request::new(request.clone())).await.unwrap();
    tokio::time::advance(std::time::Duration::from_secs(31)).await;
    fake.expire_leases().await;
    let result = fake
        .create(Request::new(request))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(result.state, AllocationState::Released as i32);
    assert_eq!(result.start_count, 1);
    assert_eq!(fake.total_starts().await, 1);
}
