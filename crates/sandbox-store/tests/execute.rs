//! PostgreSQL ownership tests. Seeded running allocations are not VM evidence.
#![allow(clippy::unwrap_used, clippy::expect_used)]
use sandbox_protocol::{
    AllocationId, HostId, Id, IdempotencyKey, OperationId, ProjectId, ProjectToken, RequestDigest,
    SandboxId, command::MAX_DURATION_MS,
};
use sandbox_store::{
    Store,
    claims::{Claim, OperationKind},
    destroy::{DestroyAdmission, DestroySandbox},
    dispatch::DispatchError,
    execute::{ExecuteAction, ExecuteAdmission, ExecuteCommand},
};
use serde_json::json;
use sqlx::PgPool;
use time::OffsetDateTime;

fn now_ms() -> i64 {
    (OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000) as i64
}
fn key() -> IdempotencyKey {
    IdempotencyKey::parse(&uuid::Uuid::now_v7().to_string()).unwrap()
}
struct Fixture {
    store: Store,
    request: ExecuteCommand,
    allocation: AllocationId,
    host: HostId,
}
impl Fixture {
    async fn new(pool: &PgPool) -> Self {
        let project = ProjectId::generate();
        let sandbox = SandboxId::generate();
        let allocation = AllocationId::generate();
        let host = HostId::generate();
        let token = ProjectToken::generate().unwrap();
        sqlx::query("INSERT INTO projects(id,name,status,limits,api_tokens) VALUES($1,'commands','active','{}',$2)")
            .bind(project.uuid()).bind(json!([{"key_id":token.key_id().as_str(),"hash":hex::encode(token.hash().as_bytes())}]))
            .execute(pool).await.unwrap();
        sqlx::query("INSERT INTO hosts(id,status,cpu_capacity,memory_capacity_mib,disk_capacity_mib,supervisor_epoch,last_seen_at) VALUES($1,'ready',4,8192,65536,1,clock_timestamp())")
            .bind(host.uuid()).execute(pool).await.unwrap();
        sqlx::query("INSERT INTO sandboxes(id,project_id,image_digest,resources,desired_state,observed_state,generation) VALUES($1,$2,'sha256:test','{}','running','running',1)")
            .bind(sandbox.uuid()).bind(project.uuid()).execute(pool).await.unwrap();
        sqlx::query("INSERT INTO allocations(id,project_id,sandbox_id,host_id,generation,supervisor_epoch,vcpu,memory_mib,disk_mib,status,lease_expires_at) VALUES($1,$2,$3,$4,1,1,1,128,64,'running',clock_timestamp()+interval '60 seconds')")
            .bind(allocation.uuid()).bind(project.uuid()).bind(sandbox.uuid()).bind(host.uuid()).execute(pool).await.unwrap();
        sqlx::query("UPDATE sandboxes SET current_allocation_id=$2 WHERE id=$1")
            .bind(sandbox.uuid())
            .bind(allocation.uuid())
            .execute(pool)
            .await
            .unwrap();
        Self {
            store: Store::from_pool(pool.clone()),
            allocation,
            host,
            request: ExecuteCommand {
                project_id: project,
                sandbox_id: sandbox,
                key_id: token.key_id().clone(),
                idempotency_key: key(),
                command: serde_json::from_value(
                    json!({"argv":["/bin/busybox","echo","private-argument"],
                "env":{"PRIVATE_VALUE":"private-environment"},"deadline_unix_ms":now_ms()+60_000}),
                )
                .unwrap(),
            },
        }
    }
    async fn admit(&self) -> OperationId {
        let ExecuteAdmission::Accepted {
            operation_id,
            status,
        } = self.store.admit_execute(&self.request).await.unwrap()
        else {
            panic!("admission")
        };
        assert_eq!(status, "queued");
        operation_id
    }
    async fn claim(&self) -> Claim {
        self.store
            .claim_next(OperationKind::Execute, 30)
            .await
            .unwrap()
            .unwrap()
    }
    async fn destroy(&self) -> OperationId {
        let request = DestroySandbox {
            project_id: self.request.project_id,
            sandbox_id: self.request.sandbox_id,
            key_id: self.request.key_id.clone(),
            idempotency_key: key(),
            request_digest: RequestDigest::compute(
                "POST",
                &format!("/v1/sandboxes/{}/destroy", self.request.sandbox_id),
                &json!({}),
            )
            .unwrap(),
            correlation_id: None,
        };
        let DestroyAdmission::Accepted { operation_id, .. } =
            self.store.admit_destroy(&request).await.unwrap()
        else {
            panic!("destroy must not wait for execute")
        };
        operation_id
    }
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn identical_admissions_converge_and_changed_input_conflicts(pool: PgPool) {
    let f = Fixture::new(&pool).await;
    let mut tasks = tokio::task::JoinSet::new();
    for _ in 0..8 {
        let store = f.store.clone();
        let r = f.request.clone();
        tasks.spawn(async move { store.admit_execute(&r).await });
    }
    let mut ids = std::collections::BTreeSet::new();
    while let Some(r) = tasks.join_next().await {
        let ExecuteAdmission::Accepted { operation_id, .. } = r.unwrap().unwrap() else {
            panic!("retry")
        };
        ids.insert(operation_id.to_string());
    }
    assert_eq!(ids.len(), 1);
    let (n,pinned,transition):(i64,uuid::Uuid,Option<uuid::Uuid>)=sqlx::query_as("SELECT count(*) OVER(),o.execution_allocation_id,s.active_transition_operation_id FROM operations o JOIN sandboxes s ON s.id=o.sandbox_id")
        .fetch_one(&pool).await.unwrap();
    assert_eq!((n, pinned, transition), (1, f.allocation.uuid(), None));
    for which in 0..5 {
        let mut r = f.request.clone();
        match which {
            0 => r.command.argv.push("changed".into()),
            1 => r.command.cwd = "/tmp".into(),
            2 => {
                r.command.env.insert("X".into(), "y".into());
            }
            3 => r.command.deadline_unix_ms += 1,
            _ => r.command.output_limit += 1,
        };
        assert_eq!(
            f.store.admit_execute(&r).await.unwrap(),
            ExecuteAdmission::DigestConflict
        );
    }
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn distinct_commands_serialize_and_unknown_retains_the_slot(pool: PgPool) {
    let f = Fixture::new(&pool).await;
    let mut tasks = tokio::task::JoinSet::new();
    for _ in 0..8 {
        let store = f.store.clone();
        let mut r = f.request.clone();
        r.idempotency_key = key();
        tasks.spawn(async move { store.admit_execute(&r).await });
    }
    let (mut accepted, mut busy) = (0, 0);
    while let Some(r) = tasks.join_next().await {
        match r.unwrap().unwrap() {
            ExecuteAdmission::Accepted { .. } => accepted += 1,
            ExecuteAdmission::Busy(_) => busy += 1,
            other => panic!("{other:?}"),
        }
    }
    assert_eq!((accepted, busy), (1, 7));
    sqlx::query("UPDATE operations SET status='unknown'")
        .execute(&pool)
        .await
        .unwrap();
    assert!(matches!(
        f.store.admit_execute(&f.request).await.unwrap(),
        ExecuteAdmission::Busy(_)
    ));
    // Even a caller bypassing store admission cannot create a second pinned command.
    let err=sqlx::query("INSERT INTO operations(id,project_id,sandbox_id,kind,initiator_kind,initiator_key_id,idempotency_key,request_digest,digest_version,payload,status,execution_allocation_id)
        SELECT $1,project_id,sandbox_id,kind,initiator_kind,initiator_key_id,$2,request_digest,digest_version,payload,'queued',execution_allocation_id FROM operations LIMIT 1")
        .bind(uuid::Uuid::now_v7()).bind(key().as_str()).execute(&pool).await.unwrap_err();
    assert_eq!(
        err.as_database_error().unwrap().constraint(),
        Some("operations_one_active_execution")
    );
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn tenant_and_authority_checks_precede_mutation(pool: PgPool) {
    let f = Fixture::new(&pool).await;
    let other = Fixture::new(&pool).await;
    let mut r = f.request.clone();
    r.sandbox_id = other.request.sandbox_id;
    assert_eq!(
        f.store.admit_execute(&r).await.unwrap(),
        ExecuteAdmission::NotFound
    );
    r.sandbox_id = SandboxId::generate();
    assert_eq!(
        f.store.admit_execute(&r).await.unwrap(),
        ExecuteAdmission::NotFound
    );
    sqlx::query("UPDATE projects SET status='suspended' WHERE id=$1")
        .bind(f.request.project_id.uuid())
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        f.store.admit_execute(&f.request).await.unwrap(),
        ExecuteAdmission::Unauthorized
    );
    let (n,): (i64,) = sqlx::query_as("SELECT count(*) FROM operations")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(n, 0);
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn invalid_input_deadline_and_unready_targets_do_not_consume_keys(pool: PgPool) {
    let f = Fixture::new(&pool).await;
    for value in [
        i64::MIN,
        now_ms() - 1,
        now_ms() + MAX_DURATION_MS + 60_000,
        i64::MAX,
    ] {
        let mut r = f.request.clone();
        r.command.deadline_unix_ms = value;
        assert_eq!(
            f.store.admit_execute(&r).await.unwrap(),
            ExecuteAdmission::InvalidDeadline
        );
    }
    for value in [0, 10 * 1024 * 1024 + 1] {
        let mut r = f.request.clone();
        r.command.output_limit = value;
        assert_eq!(
            f.store.admit_execute(&r).await.unwrap(),
            ExecuteAdmission::InvalidCommand
        );
    }
    let mut r = f.request.clone();
    r.command.env.insert("BAD=NAME".into(), "value".into());
    assert_eq!(
        f.store.admit_execute(&r).await.unwrap(),
        ExecuteAdmission::InvalidCommand
    );
    sqlx::query("UPDATE allocations SET lease_expires_at=clock_timestamp()-interval '1 second'")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        f.store.admit_execute(&f.request).await.unwrap(),
        ExecuteAdmission::NotRunning
    );
    sqlx::query("UPDATE allocations SET lease_expires_at=clock_timestamp()+interval '60 seconds'")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE sandboxes SET expires_at=clock_timestamp()+interval '1 second'")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        f.store.admit_execute(&f.request).await.unwrap(),
        ExecuteAdmission::InvalidDeadline
    );
    sqlx::query("UPDATE sandboxes SET expires_at=NULL")
        .execute(&pool)
        .await
        .unwrap();
    f.admit().await;
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn retries_survive_expiry_destroy_and_target_loss(pool: PgPool) {
    let f = Fixture::new(&pool).await;
    let id = f.admit().await;
    f.destroy().await;
    let retry = f.store.admit_execute(&f.request).await.unwrap();
    assert_eq!(
        retry,
        ExecuteAdmission::Accepted {
            operation_id: id,
            status: "queued".into()
        }
    );
    let mut new = f.request.clone();
    new.idempotency_key = key();
    assert_eq!(
        f.store.admit_execute(&new).await.unwrap(),
        ExecuteAdmission::Gone
    );
    // No real wait: persisted request was admitted; database deadline is now expired.
    sqlx::query(
        "UPDATE operations SET deadline=clock_timestamp()-interval '1 second' WHERE kind='execute'",
    )
    .execute(&pool)
    .await
    .unwrap();
    assert_eq!(f.store.admit_execute(&f.request).await.unwrap(), retry);
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn dispatch_intent_commits_once_and_reconciliation_stays_pinned(pool: PgPool) {
    let f = Fixture::new(&pool).await;
    let id = f.admit().await;
    let claim = f.claim().await;
    let ExecuteAction::Dispatch { owner, command } =
        f.store.prepare_execute(&claim, f.host, 1).await.unwrap()
    else {
        panic!("dispatch")
    };
    assert_eq!(owner.allocation_id, f.allocation.to_string());
    assert_eq!(command.operation_id, id);
    assert_eq!(command.argv, f.request.command.argv);
    let digest = command.digest().unwrap();
    let (attempts, receipts): (i32, serde_json::Value) =
        sqlx::query_as("SELECT attempt_count,attempt_receipts FROM operations WHERE id=$1")
            .bind(id.uuid())
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(attempts, 1);
    assert_eq!(receipts[0]["command_digest"], hex::encode(digest));
    assert!(!receipts.to_string().contains("private-"));
    assert!(!format!("{claim:?}").contains("private-"));
    assert!(!format!("{:?}", f.request).contains("private-"));
    f.destroy().await;
    sqlx::query("UPDATE allocations SET status='released',released_at=clock_timestamp(),release_evidence='{}'").execute(&pool).await.unwrap();
    sqlx::query("UPDATE sandboxes SET current_allocation_id=NULL")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE projects SET status='suspended'")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE hosts SET supervisor_epoch=2")
        .execute(&pool)
        .await
        .unwrap();
    // The replacement claim inspects the original allocation/epoch even when the
    // runtime is gone. None of these events is evidence of a command exit.
    sqlx::query("UPDATE operations SET status='unknown',lease_expires_at=NULL,deadline=clock_timestamp()-interval '1 second' WHERE id=$1")
        .bind(id.uuid()).execute(&pool).await.unwrap();
    let next = f.claim().await;
    let ExecuteAction::Inspect {
        owner: after,
        digest: after_digest,
    } = f.store.prepare_execute(&next, f.host, 2).await.unwrap()
    else {
        panic!("inspect only")
    };
    assert_eq!(after.allocation_id, owner.allocation_id);
    assert_eq!(after.generation, 1);
    assert_eq!(after.supervisor_epoch, 1);
    assert_eq!(after_digest, digest);
    assert!(after.claim_revision > owner.claim_revision);
    assert!(matches!(
        f.store.prepare_execute(&claim, f.host, 1).await,
        Err(DispatchError::LostClaim)
    ));
    let (attempts,): (i32,) = sqlx::query_as("SELECT attempt_count FROM operations WHERE id=$1")
        .bind(id.uuid())
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(attempts, 1);
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn authority_expiry_and_destroy_reject_only_undispatched_commands(pool: PgPool) {
    for scenario in 0..4 {
        let f = Fixture::new(&pool).await;
        let id = f.admit().await;
        let claim = f.claim().await;
        match scenario {
            0 => {
                sqlx::query("UPDATE projects SET status='suspended' WHERE id=$1")
                    .bind(f.request.project_id.uuid())
                    .execute(&pool)
                    .await
                    .unwrap();
            }
            1 => {
                sqlx::query("UPDATE operations SET deadline=clock_timestamp()-interval '1 second' WHERE id=$1").bind(id.uuid()).execute(&pool).await.unwrap();
            }
            2 => {
                f.destroy().await;
            }
            _ => {
                sqlx::query("UPDATE projects SET api_tokens=jsonb_set(api_tokens,'{0,revoked_at}',to_jsonb(clock_timestamp()::text)) WHERE id=$1").bind(f.request.project_id.uuid()).execute(&pool).await.unwrap();
            }
        }
        assert!(matches!(
            f.store.prepare_execute(&claim, f.host, 1).await.unwrap(),
            ExecuteAction::Rejected
        ));
        let (status,attempts,unreleased):(String,i32,bool)=sqlx::query_as("SELECT o.status,o.attempt_count,a.released_at IS NULL FROM operations o JOIN allocations a ON a.id=o.execution_allocation_id WHERE o.id=$1")
            .bind(id.uuid()).fetch_one(&pool).await.unwrap();
        assert_eq!((status.as_str(), attempts, unreleased), ("failed", 0, true));
    }
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn wrong_host_stale_lease_and_corrupt_payload_never_dispatch(pool: PgPool) {
    let f = Fixture::new(&pool).await;
    let id = f.admit().await;
    let claim = f.claim().await;
    assert!(matches!(
        f.store.prepare_execute(&claim, HostId::generate(), 1).await,
        Err(DispatchError::HostUnavailable)
    ));
    assert!(matches!(
        f.store.prepare_execute(&claim, f.host, 2).await,
        Err(DispatchError::HostUnavailable)
    ));
    sqlx::query("UPDATE allocations SET lease_expires_at=clock_timestamp()-interval '1 second'")
        .execute(&pool)
        .await
        .unwrap();
    assert!(matches!(
        f.store.prepare_execute(&claim, f.host, 1).await,
        Err(DispatchError::HostUnavailable)
    ));
    sqlx::query("UPDATE allocations SET lease_expires_at=clock_timestamp()+interval '60 seconds'")
        .execute(&pool)
        .await
        .unwrap();
    assert!(matches!(
        f.store.prepare_execute(&claim, f.host, 1).await.unwrap(),
        ExecuteAction::Dispatch { .. }
    ));
    sqlx::query(
        "UPDATE operations SET payload=jsonb_set(payload,'{argv,2}','\"changed\"') WHERE id=$1",
    )
    .bind(id.uuid())
    .execute(&pool)
    .await
    .unwrap();
    assert!(matches!(
        f.store.prepare_execute(&claim, f.host, 1).await,
        Err(DispatchError::InvalidData)
    ));
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn execution_pin_cannot_cross_sandboxes(pool: PgPool) {
    let f = Fixture::new(&pool).await;
    let other = Fixture::new(&pool).await;
    let id = f.admit().await;
    let error = sqlx::query("UPDATE operations SET execution_allocation_id=$2 WHERE id=$1")
        .bind(id.uuid())
        .bind(other.allocation.uuid())
        .execute(&pool)
        .await
        .unwrap_err();
    assert_eq!(
        error.as_database_error().unwrap().constraint(),
        Some("operations_execution_allocation_same_sandbox")
    );
    let error = sqlx::query("UPDATE operations SET kind='create' WHERE id=$1")
        .bind(id.uuid())
        .execute(&pool)
        .await
        .unwrap_err();
    assert_eq!(
        error.as_database_error().unwrap().constraint(),
        Some("operations_execution_allocation_kind")
    );
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn admission_rechecks_time_after_host_lock_wait(pool: PgPool) {
    let f = Fixture::new(&pool).await;
    sqlx::query("UPDATE projects SET api_tokens=jsonb_set(api_tokens,'{0,expires_at}',to_jsonb((clock_timestamp()+interval '1 second')::text))")
        .execute(&pool).await.unwrap();
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("SELECT id FROM hosts WHERE id=$1 FOR UPDATE")
        .bind(f.host.uuid())
        .execute(&mut *tx)
        .await
        .unwrap();
    let store = f.store.clone();
    let r = f.request.clone();
    let task = tokio::spawn(async move { store.admit_execute(&r).await });
    tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
    tx.commit().await.unwrap();
    assert!(matches!(
        task.await.unwrap(),
        Err(DispatchError::Conflict) | Ok(ExecuteAdmission::Unauthorized)
    ));
    let (n,): (i64,) = sqlx::query_as("SELECT count(*) FROM operations")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(n, 0);
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn dispatch_rechecks_claim_after_host_lock_wait(pool: PgPool) {
    let f = Fixture::new(&pool).await;
    let id = f.admit().await;
    let claim = f
        .store
        .claim_next(OperationKind::Execute, 1)
        .await
        .unwrap()
        .unwrap();
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("SELECT id FROM hosts WHERE id=$1 FOR UPDATE")
        .bind(f.host.uuid())
        .execute(&mut *tx)
        .await
        .unwrap();
    let host = f.host;
    let store = f.store.clone();
    let task = tokio::spawn(async move { store.prepare_execute(&claim, host, 1).await });
    tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
    tx.commit().await.unwrap();
    assert!(matches!(task.await.unwrap(), Err(DispatchError::LostClaim)));
    let (n,): (i32,) = sqlx::query_as("SELECT attempt_count FROM operations WHERE id=$1")
        .bind(id.uuid())
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(n, 0);
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn normalized_input_retries_after_actual_command_expiry(pool: PgPool) {
    let mut f = Fixture::new(&pool).await;
    f.request.command.deadline_unix_ms = now_ms() + 1000;
    f.request.command.env = std::collections::BTreeMap::new();
    let id = f.admit().await;
    let mut retry = f.request.clone();
    retry.command =
        serde_json::from_value(json!({"argv":f.request.command.argv,"env":{},"cwd":"/",
        "deadline_unix_ms":f.request.command.deadline_unix_ms,"output_limit":1048576}))
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    assert_eq!(
        f.store.admit_execute(&retry).await.unwrap(),
        ExecuteAdmission::Accepted {
            operation_id: id,
            status: "queued".into()
        }
    );
    retry.idempotency_key = key();
    // Release the slot as a controller would after rejecting an expired intent.
    let claim = f.claim().await;
    assert!(matches!(
        f.store.prepare_execute(&claim, f.host, 1).await.unwrap(),
        ExecuteAction::Rejected
    ));
    assert_eq!(
        f.store.admit_execute(&retry).await.unwrap(),
        ExecuteAdmission::InvalidDeadline
    );
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn create_and_execute_share_the_project_idempotency_namespace(pool: PgPool) {
    use sandbox_store::admission::{Admission, CreateSandbox, Resources};
    let f = Fixture::new(&pool).await;
    let image = format!("sha256:{}", "a".repeat(64));
    let payload = json!({"image_digest":image});
    let create = CreateSandbox {
        project_id: f.request.project_id,
        key_id: f.request.key_id.clone(),
        idempotency_key: f.request.idempotency_key.clone(),
        request_digest: RequestDigest::compute("POST", "/v1/sandboxes", &payload).unwrap(),
        image_digest: image.clone(),
        name: None,
        resources: Resources {
            vcpu: 1,
            memory_mib: 128,
            disk_mib: 64,
        },
        payload,
    };
    let images = sandbox_protocol::images::ImageAllowlist::new([image]).unwrap();
    let (create, execute) = tokio::join!(
        f.store.admit_create_sandbox(&create, &images),
        f.store.admit_execute(&f.request)
    );
    assert!(matches!(
        (create.unwrap(), execute.unwrap()),
        (Admission::Admitted { .. }, ExecuteAdmission::DigestConflict)
            | (
                Admission::DigestConflict { .. },
                ExecuteAdmission::Accepted { .. }
            )
    ));
    let (n,): (i64,) = sqlx::query_as(
        "SELECT count(*) FROM operations WHERE project_id=$1 AND idempotency_key=$2",
    )
    .bind(f.request.project_id.uuid())
    .bind(f.request.idempotency_key.as_str())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(n, 1);
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn missing_intent_or_changed_allocation_tuple_cannot_authorize_recovery(pool: PgPool) {
    for scenario in 0..3 {
        let f = Fixture::new(&pool).await;
        let id = f.admit().await;
        let claim = f.claim().await;
        if scenario == 0 {
            sqlx::query("UPDATE operations SET status='unknown' WHERE id=$1")
                .bind(id.uuid())
                .execute(&pool)
                .await
                .unwrap();
        } else {
            assert!(matches!(
                f.store.prepare_execute(&claim, f.host, 1).await.unwrap(),
                ExecuteAction::Dispatch { .. }
            ));
            if scenario == 1 {
                sqlx::query("UPDATE allocations SET supervisor_epoch=2 WHERE id=$1")
                    .bind(f.allocation.uuid())
                    .execute(&pool)
                    .await
                    .unwrap();
            } else {
                sqlx::query("UPDATE allocations SET generation=2 WHERE id=$1")
                    .bind(f.allocation.uuid())
                    .execute(&pool)
                    .await
                    .unwrap();
            }
        }
        assert!(matches!(
            f.store.prepare_execute(&claim, f.host, 1).await,
            Err(DispatchError::InvalidData)
        ));
        sqlx::query(
            "UPDATE operations SET status='failed',completed_at=clock_timestamp() WHERE id=$1",
        )
        .bind(id.uuid())
        .execute(&pool)
        .await
        .unwrap();
    }
}
