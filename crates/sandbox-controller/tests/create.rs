//! PostgreSQL + the actual HTTP router + loopback gRPC/mTLS. No VM is used.
#![allow(clippy::unwrap_used, clippy::expect_used)]
use axum::{
    Router,
    body::{Body, to_bytes},
};
use http::{Request, StatusCode};
use rcgen::{
    BasicConstraints, CertificateParams, CertifiedIssuer, ExtendedKeyUsagePurpose, IsCa, KeyPair,
    KeyUsagePurpose,
};
use sandbox_api::{AppState, router};
use sandbox_controller::{ControllerConfig, ControllerError, CreateController, Tick};
use sandbox_fake_host::{FakeConfig, FakeHost};
use sandbox_protocol::{
    HostId, Id, OperationId, ProjectId, ProjectToken, SandboxId,
    supervisor::{
        CreateRequest, StopRequest,
        supervisor_server::{Supervisor, SupervisorServer},
    },
};
use sandbox_store::{
    Store,
    claims::{Claim, OperationKind},
    dispatch::{CreateAction, CreateRejection, DispatchError},
};
use sandbox_supervisor::transport::{self, ControllerIdentity, MAX_MESSAGE_BYTES};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use std::collections::BTreeSet;
use tokio::{net::TcpListener, task::JoinHandle};
use tokio_stream::wrappers::TcpListenerStream;
use tonic::{service::interceptor::InterceptedService, transport::Server};
use tower::ServiceExt;

struct Fixture {
    store: Store,
    app: Router,
    token: String,
    config: ControllerConfig,
    ca: String,
    cert: String,
    key: String,
    fake: FakeHost,
    task: JoinHandle<()>,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Fixture {
    async fn new(pool: &PgPool) -> Self {
        let project = ProjectId::generate();
        let token = ProjectToken::generate().unwrap();
        sqlx::query("INSERT INTO projects(id,name,status,limits,api_tokens) VALUES($1,'controller-test','active','{}',$2)")
            .bind(project.uuid()).bind(json!([{"key_id":token.key_id().as_str(),"hash":hex::encode(token.hash().as_bytes())}])).execute(pool).await.unwrap();
        let host = HostId::generate();
        sqlx::query("INSERT INTO hosts(id,status,cpu_capacity,memory_capacity_mib,disk_capacity_mib,supervisor_epoch) VALUES($1,'ready',4,8192,65536,1)")
            .bind(host.uuid()).execute(pool).await.unwrap();
        let digest = format!("sha256:{}", "a".repeat(64));
        let mut ca_params = CertificateParams::new(Vec::new()).unwrap();
        ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        ca_params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        let ca = CertifiedIssuer::self_signed(ca_params, KeyPair::generate().unwrap()).unwrap();
        let controller_key = KeyPair::generate().unwrap();
        let mut controller_params =
            CertificateParams::new(vec!["controller.sandbox.internal".into()]).unwrap();
        controller_params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
        let controller_cert = controller_params.signed_by(&controller_key, &ca).unwrap();
        let server_key = KeyPair::generate().unwrap();
        let mut server_params =
            CertificateParams::new(vec![transport::host_server_name(host)]).unwrap();
        server_params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        let server_cert = server_params.signed_by(&server_key, &ca).unwrap();
        let fake = FakeHost::new(FakeConfig {
            host,
            epoch: 1,
            images: BTreeSet::from([digest.clone()]),
            capacity: sandbox_protocol::supervisor::Resources {
                vcpu: 4,
                memory_mib: 8192,
                disk_mib: 65536,
            },
        })
        .unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("https://{}", listener.local_addr().unwrap());
        let service = SupervisorServer::new(fake.clone())
            .max_decoding_message_size(MAX_MESSAGE_BYTES)
            .max_encoding_message_size(MAX_MESSAGE_BYTES);
        let service = InterceptedService::new(
            service,
            ControllerIdentity::new(vec![Sha256::digest(controller_cert.der()).into()]).unwrap(),
        );
        let tls = transport::server_tls(
            ca.pem().as_bytes(),
            server_cert.pem().as_bytes(),
            server_key.serialize_pem().as_bytes(),
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
        let store = Store::from_pool(pool.clone());
        Self {
            app: router(AppState {
                store: store.clone(),
            }),
            store,
            token: token.render_once(),
            config: ControllerConfig {
                endpoint,
                host,
                epoch: 1,
                allowed_images: BTreeSet::from([digest]),
                allow_simulated: true,
            },
            ca: ca.pem(),
            cert: controller_cert.pem(),
            key: controller_key.serialize_pem(),
            fake,
            task,
        }
    }
    async fn controller(&self) -> CreateController {
        CreateController::connect(
            self.store.clone(),
            self.config.clone(),
            self.ca.as_bytes(),
            self.cert.as_bytes(),
            self.key.as_bytes(),
        )
        .await
        .unwrap()
    }
    async fn send(&self, method: &str, path: &str, body: Value) -> (StatusCode, Value) {
        let request = Request::builder()
            .method(method)
            .uri(path)
            .header("authorization", format!("Bearer {}", self.token))
            .header("content-type", "application/json")
            .header("idempotency-key", uuid::Uuid::now_v7().to_string())
            .body(Body::from(body.to_string()))
            .unwrap();
        let response = self.app.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        (status, serde_json::from_slice(&body).unwrap())
    }
    async fn admit(&self) -> (OperationId, SandboxId) {
        let (status, body) = self
            .send(
                "POST",
                "/v1/sandboxes",
                json!({"image_digest":format!("sha256:{}","a".repeat(64)),
            "resources":{"vcpu":2,"memory_mib":2048,"disk_mib":8192}}),
            )
            .await;
        assert_eq!(status, StatusCode::ACCEPTED, "{body}");
        (
            body["operation_id"].as_str().unwrap().parse().unwrap(),
            body["sandbox_id"].as_str().unwrap().parse().unwrap(),
        )
    }
    async fn reclaim_now(&self) {
        sqlx::query("UPDATE operations SET lease_expires_at=clock_timestamp()-interval '1 second',next_retry_at=NULL WHERE completed_at IS NULL")
            .execute(self.store.pool()).await.unwrap();
    }
    async fn prepared(&self) -> (Claim, CreateRequest) {
        self.store
            .observe_configured_host(self.config.host, 1)
            .await
            .unwrap();
        let claim = self
            .store
            .claim_next(OperationKind::Create, 30)
            .await
            .unwrap()
            .unwrap();
        self.store
            .reserve_create(&claim, self.config.host, 1)
            .await
            .unwrap();
        let CreateAction::Start(request) = self
            .store
            .prepare_create_dispatch(&claim, &self.config.allowed_images)
            .await
            .unwrap()
        else {
            panic!("fresh start")
        };
        (claim, request)
    }
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn admitted_http_create_reaches_confirmed_simulated_state(pool: PgPool) {
    let f = Fixture::new(&pool).await;
    let (operation, sandbox) = f.admit().await;
    let mut controller = f.controller().await;
    assert_eq!(controller.tick().await.unwrap(), Tick::Confirmed);
    let (status, body) = f
        .send("GET", &format!("/v1/operations/{operation}"), Value::Null)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "succeeded");
    assert_eq!(body["result"]["simulated"], true);
    assert!(body["completed_at"].is_string());
    let (_, body) = f
        .send("GET", &format!("/v1/sandboxes/{sandbox}"), Value::Null)
        .await;
    assert_eq!(body["observed_state"], "running");
    assert_eq!(body["observation_simulated"], true);
    assert!(body["observed_at"].is_string());
    assert!(body.get("active_operation_id").is_none());
    assert_eq!(controller.tick().await.unwrap(), Tick::Idle);
    assert_eq!(f.fake.total_starts().await, 1);
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn lost_rpc_reply_is_reconciled_from_the_same_single_start(pool: PgPool) {
    let f = Fixture::new(&pool).await;
    let (operation, _) = f.admit().await;
    let mut controller = f.controller().await;
    f.fake.lose_next_create_reply().await;
    assert_eq!(controller.tick().await.unwrap(), Tick::Unknown);
    let (_, body) = f
        .send("GET", &format!("/v1/operations/{operation}"), Value::Null)
        .await;
    assert_eq!(body["status"], "unknown");
    assert!(body.get("completed_at").is_none());
    let (count,): (i64,) =
        sqlx::query_as("SELECT count(*) FROM allocations WHERE released_at IS NULL")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(count, 1);
    // Revocation prevents new dispatch, but cannot erase already-applied work.
    sqlx::query("UPDATE projects SET api_tokens='[]'")
        .execute(&pool)
        .await
        .unwrap();
    f.reclaim_now().await;
    assert_eq!(f.controller().await.tick().await.unwrap(), Tick::Confirmed);
    assert_eq!(f.fake.total_starts().await, 1);
    let (attempts, status, error): (i32, String, Option<Value>) =
        sqlx::query_as("SELECT attempt_count,status,error FROM operations")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(attempts, 1);
    assert_eq!(status, "succeeded");
    assert!(error.is_none());
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn crash_after_intent_before_send_remains_unknown_without_replay(pool: PgPool) {
    let f = Fixture::new(&pool).await;
    f.admit().await;
    f.prepared().await;
    f.reclaim_now().await;
    let mut controller = f.controller().await;
    for _ in 0..2 {
        assert_eq!(controller.tick().await.unwrap(), Tick::Unknown);
        f.reclaim_now().await;
    }
    assert_eq!(f.fake.total_starts().await, 0);
    let (attempts, receipts): (i32, Value) =
        sqlx::query_as("SELECT attempt_count,attempt_receipts FROM operations")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(attempts, 1);
    assert_eq!(receipts.as_array().unwrap().len(), 2);
    assert_eq!(receipts[1]["phase"], "create_dispatch_intent");
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn crash_before_intent_can_continue_from_proven_undispatched_reservation(pool: PgPool) {
    let f = Fixture::new(&pool).await;
    f.admit().await;
    let mut controller = f.controller().await;
    let old = f
        .store
        .claim_next(OperationKind::Create, 30)
        .await
        .unwrap()
        .unwrap();
    f.store
        .reserve_create(&old, f.config.host, 1)
        .await
        .unwrap();
    f.reclaim_now().await;
    assert_eq!(controller.tick().await.unwrap(), Tick::Confirmed);
    assert_eq!(f.fake.total_starts().await, 1);
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn multiple_controllers_cannot_dispatch_the_same_create(pool: PgPool) {
    let f = Fixture::new(&pool).await;
    f.admit().await;
    let mut tasks = tokio::task::JoinSet::new();
    for _ in 0..8 {
        let mut controller = f.controller().await;
        tasks.spawn(async move { controller.tick().await.unwrap() });
    }
    let mut confirmed = 0;
    while let Some(result) = tasks.join_next().await {
        if result.unwrap() == Tick::Confirmed {
            confirmed += 1;
        }
    }
    assert_eq!(confirmed, 1);
    assert_eq!(f.fake.total_starts().await, 1);
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn simulated_host_requires_explicit_opt_in_before_claiming(pool: PgPool) {
    let mut f = Fixture::new(&pool).await;
    f.admit().await;
    f.config.allow_simulated = false;
    assert!(matches!(
        CreateController::connect(
            f.store.clone(),
            f.config.clone(),
            f.ca.as_bytes(),
            f.cert.as_bytes(),
            f.key.as_bytes()
        )
        .await,
        Err(ControllerError::HostIdentity)
    ));
    let (status,): (String,) = sqlx::query_as("SELECT status FROM operations")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(status, "queued");
    assert_eq!(f.fake.total_starts().await, 0);
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn denied_images_release_only_undispatched_reservations(pool: PgPool) {
    let mut f = Fixture::new(&pool).await;
    f.admit().await;
    f.config.allowed_images = BTreeSet::from([format!("sha256:{}", "b".repeat(64))]);
    assert_eq!(f.controller().await.tick().await.unwrap(), Tick::Rejected);
    assert_eq!(f.fake.total_starts().await, 0);
    let (status, attempts): (String, i32) =
        sqlx::query_as("SELECT status,attempt_count FROM operations")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(status, "failed");
    assert_eq!(attempts, 0);
    let (proof,): (Value,) =
        sqlx::query_as("SELECT release_evidence FROM allocations WHERE released_at IS NOT NULL")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(proof["dispatch_intent_absent"], true);
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn revoked_project_cannot_dispatch_or_leave_an_undispatched_allocation(pool: PgPool) {
    let f = Fixture::new(&pool).await;
    f.admit().await;
    sqlx::query("UPDATE projects SET api_tokens='[]'")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(f.controller().await.tick().await.unwrap(), Tick::Rejected);
    assert_eq!(f.fake.total_starts().await, 0);
    let (status,): (String,) = sqlx::query_as("SELECT status FROM operations")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(status, "failed");
    let (count,): (i64,) = sqlx::query_as("SELECT count(*) FROM allocations")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn mismatched_or_stale_evidence_cannot_commit_success(pool: PgPool) {
    let f = Fixture::new(&pool).await;
    f.admit().await;
    let (claim, request) = f.prepared().await;
    let observation = f
        .fake
        .create(tonic::Request::new(request))
        .await
        .unwrap()
        .into_inner();
    for field in [
        "project",
        "sandbox",
        "host",
        "allocation",
        "operation",
        "epoch",
        "generation",
        "revision",
        "deadline",
        "create_operation",
    ] {
        let mut wrong = observation.clone();
        let owner = wrong.ownership.as_mut().unwrap();
        match field {
            "project" => owner.project_id = ProjectId::generate().to_string(),
            "sandbox" => owner.sandbox_id = SandboxId::generate().to_string(),
            "host" => owner.host_id = HostId::generate().to_string(),
            "allocation" => owner.allocation_id = "wrong".into(),
            "operation" => owner.operation_id = OperationId::generate().to_string(),
            "epoch" => owner.supervisor_epoch += 1,
            "generation" => owner.generation += 1,
            "revision" => owner.claim_revision += 1,
            "deadline" => owner.claim_expires_unix_ms += 1,
            _ => wrong.create_operation_id = OperationId::generate().to_string(),
        }
        assert!(
            matches!(
                f.store
                    .record_create_observation(&claim, &wrong, true)
                    .await,
                Err(DispatchError::BadEvidence)
            ),
            "{field}"
        );
    }
    assert!(matches!(
        f.store
            .record_create_observation(&claim, &observation, false)
            .await,
        Err(DispatchError::SimulationDenied)
    ));
    f.reclaim_now().await;
    f.store
        .claim_next(OperationKind::Create, 30)
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        f.store
            .record_create_observation(&claim, &observation, true)
            .await,
        Err(DispatchError::LostClaim)
    ));
    assert!(
        f.store
            .reject_undispatched_create(&claim, CreateRejection::Unauthorized)
            .await
            .is_err()
    );
    let (status,): (String,) = sqlx::query_as("SELECT status FROM operations")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(status, "running");
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn expired_readiness_is_not_release_but_matching_stop_evidence_is(pool: PgPool) {
    let f = Fixture::new(&pool).await;
    f.admit().await;
    let (claim, request) = f.prepared().await;
    let ownership = request.ownership.clone();
    let observation = f
        .fake
        .create(tonic::Request::new(request))
        .await
        .unwrap()
        .into_inner();
    sqlx::query("UPDATE allocations SET lease_expires_at=clock_timestamp()-interval '1 second'")
        .execute(&pool)
        .await
        .unwrap();
    assert!(matches!(
        f.store
            .record_create_observation(&claim, &observation, true)
            .await,
        Err(DispatchError::BadEvidence)
    ));
    assert!(
        f.store
            .reject_undispatched_create(&claim, CreateRejection::Unauthorized)
            .await
            .is_err()
    );
    let stopped = f
        .fake
        .stop(tonic::Request::new(StopRequest { ownership }))
        .await
        .unwrap()
        .into_inner();
    f.store
        .record_create_observation(&claim, &stopped, true)
        .await
        .unwrap();
    let (status,): (String,) = sqlx::query_as("SELECT status FROM operations")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(status, "failed");
    let (count,): (i64,) =
        sqlx::query_as("SELECT count(*) FROM allocations WHERE released_at IS NULL")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(count, 0);
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn authority_is_rechecked_after_reservation_and_before_rpc(pool: PgPool) {
    let f = Fixture::new(&pool).await;
    f.admit().await;
    let mut controller = f.controller().await;
    let claim = f
        .store
        .claim_next(OperationKind::Create, 30)
        .await
        .unwrap()
        .unwrap();
    f.store
        .reserve_create(&claim, f.config.host, 1)
        .await
        .unwrap();
    sqlx::query("UPDATE projects SET status='suspended'")
        .execute(&pool)
        .await
        .unwrap();
    f.reclaim_now().await;
    assert_eq!(controller.tick().await.unwrap(), Tick::Rejected);
    assert_eq!(f.fake.total_starts().await, 0);
    let (count,): (i64,) =
        sqlx::query_as("SELECT count(*) FROM allocations WHERE released_at IS NULL")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(count, 0);
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn claim_expiry_during_completion_lock_wait_rolls_back_every_write(pool: PgPool) {
    let f = Fixture::new(&pool).await;
    f.admit().await;
    f.store
        .observe_configured_host(f.config.host, 1)
        .await
        .unwrap();
    let claim = f
        .store
        .claim_next(OperationKind::Create, 1)
        .await
        .unwrap()
        .unwrap();
    f.store
        .reserve_create(&claim, f.config.host, 1)
        .await
        .unwrap();
    let CreateAction::Start(request) = f
        .store
        .prepare_create_dispatch(&claim, &f.config.allowed_images)
        .await
        .unwrap()
    else {
        panic!("start")
    };
    let observation = f
        .fake
        .create(tonic::Request::new(request))
        .await
        .unwrap()
        .into_inner();
    let mut lock = pool.begin().await.unwrap();
    sqlx::query("SELECT id FROM projects FOR UPDATE")
        .execute(&mut *lock)
        .await
        .unwrap();
    let store = f.store.clone();
    let task = tokio::spawn(async move {
        store
            .record_create_observation(&claim, &observation, true)
            .await
    });
    tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
    lock.commit().await.unwrap();
    assert!(matches!(task.await.unwrap(), Err(DispatchError::LostClaim)));
    let (state, source): (String, Option<bool>) =
        sqlx::query_as("SELECT observed_state,observation_simulated FROM sandboxes")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(state, "creating");
    assert_eq!(source, None);
    let (status,): (String,) = sqlx::query_as("SELECT status FROM operations")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(status, "running");
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn stale_or_future_observation_time_cannot_confirm_readiness(pool: PgPool) {
    let f = Fixture::new(&pool).await;
    f.admit().await;
    let (claim, request) = f.prepared().await;
    let original = f
        .fake
        .create(tonic::Request::new(request))
        .await
        .unwrap()
        .into_inner();
    for at in [1, i64::MAX] {
        let mut observation = original.clone();
        observation.observed_unix_ms = at;
        assert!(matches!(
            f.store
                .record_create_observation(&claim, &observation, true)
                .await,
            Err(DispatchError::BadEvidence)
        ));
    }
    f.store
        .record_create_observation(&claim, &original, true)
        .await
        .unwrap();
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn host_health_cannot_change_epoch_or_promote_a_draining_host(pool: PgPool) {
    let mut f = Fixture::new(&pool).await;
    f.admit().await;
    f.config.epoch = 2;
    assert!(matches!(
        CreateController::connect(
            f.store.clone(),
            f.config.clone(),
            f.ca.as_bytes(),
            f.cert.as_bytes(),
            f.key.as_bytes()
        )
        .await,
        Err(ControllerError::HostIdentity)
    ));
    f.config.epoch = 1;
    sqlx::query("UPDATE hosts SET status='draining'")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(f.controller().await.tick().await.unwrap(), Tick::Deferred);
    let (status, epoch): (String, i64) =
        sqlx::query_as("SELECT status,supervisor_epoch FROM hosts")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(status, "draining");
    assert_eq!(epoch, 1);
    assert_eq!(f.fake.total_starts().await, 0);
}
