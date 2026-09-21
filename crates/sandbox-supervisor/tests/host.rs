//! Real mTLS lifecycle RPCs against a guardian and bootstrapped Firecracker guest.
#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used, clippy::expect_used)]
#[path = "../../sandbox-protocol/tests/support/guest_tls.rs"]
mod tls;
#[path = "support/vm.rs"]
mod vm;
use sandbox_protocol::{
    AllocationId, Id, OperationId, SandboxId,
    supervisor::{
        AllocationState, CreateRequest, HealthRequest, InspectRequest, LeaseInspection,
        LeaseOwnership, LeaseRequest, Ownership, Resources, StopRequest,
        supervisor_client::SupervisorClient,
    },
};
use sandbox_supervisor::{
    guardian::{self, Action, Manifest},
    host::{Capacity, Config, Image},
    transport,
};
use std::{
    collections::BTreeMap,
    fs,
    path::PathBuf,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};
use tonic::{Code, transport::Channel};

struct Fixture {
    vm: vm::Fixture,
    config: Config,
    config_path: PathBuf,
    tls: tls::Fixture,
    child: Child,
    url: String,
    address: String,
}
impl Fixture {
    async fn new() -> Self {
        Self::with_agent(true).await
    }
    async fn with_agent(agent: bool) -> Self {
        let vm = vm::Fixture::build(60000, agent);
        let c = &vm.manifest.config;
        let config = Config {
            host: vm.manifest.start.owner.host,
            epoch: 1,
            state_root: vm.temp.path().join("h"),
            cgroup_parent: c.cgroup_parent.clone(),
            guardian_binary: PathBuf::from(env!("CARGO_BIN_EXE_sandbox-supervisor")),
            firecracker: c.firecracker.clone(),
            jailer: c.jailer.clone(),
            images: BTreeMap::from([(
                format!("sha256:{}", c.rootfs.sha256),
                Image {
                    kernel: c.kernel.clone(),
                    rootfs: c.rootfs.clone(),
                },
            )]),
            jail_uid: c.jail_uid,
            jail_gid: c.jail_gid,
            capacity: Capacity {
                vcpu: 2,
                memory_mib: 512,
                disk_mib: 1024,
            },
        };
        let tls = tls::Fixture::new();
        let server = tls::Leaf::new(&tls.ca, transport::host_server_name(config.host), false);
        for (name, value) in [
            ("ca.pem", tls.ca.pem()),
            ("server.pem", server.cert.pem()),
            ("server.key", server.key.serialize_pem()),
        ] {
            fs::write(vm.temp.path().join(name), value).unwrap();
        }
        let config_path = vm.temp.path().join("host-config.json");
        fs::write(&config_path, serde_json::to_vec(&config).unwrap()).unwrap();
        let address = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .to_string();
        let child = Self::spawn(&vm, &config_path, &address, &tls);
        let f = Self {
            vm,
            config,
            config_path,
            tls,
            child,
            url: format!("https://{address}"),
            address,
        };
        f.client().await;
        f
    }
    fn spawn(vm: &vm::Fixture, config: &PathBuf, address: &str, tls: &tls::Fixture) -> Child {
        Command::new(env!("CARGO_BIN_EXE_sandbox-host"))
            .arg("--config")
            .arg(config)
            .arg("--listen")
            .arg(address)
            .arg("--ca-cert")
            .arg(vm.temp.path().join("ca.pem"))
            .arg("--server-cert")
            .arg(vm.temp.path().join("server.pem"))
            .arg("--server-key")
            .arg(vm.temp.path().join("server.key"))
            .arg("--controller-cert-sha256")
            .arg(hex::encode(tls.host.pin()))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap()
    }
    async fn client(&self) -> SupervisorClient<Channel> {
        let until = Instant::now() + Duration::from_secs(10);
        loop {
            if let Ok(mut c) = transport::connect(
                &self.url,
                self.config.host,
                self.tls.ca.pem().as_bytes(),
                self.tls.host.cert.pem().as_bytes(),
                self.tls.host.key.serialize_pem().as_bytes(),
            )
            .await
                && c.health(HealthRequest {}).await.is_ok()
            {
                return c;
            }
            assert!(Instant::now() < until, "host never healthy");
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }
    fn request(&self) -> CreateRequest {
        let owner = &self.vm.manifest.start.owner;
        CreateRequest {
            ownership: Some(Ownership {
                host_id: self.config.host.to_string(),
                project_id: owner.project.to_string(),
                sandbox_id: SandboxId::generate().to_string(),
                allocation_id: AllocationId::generate().to_string(),
                operation_id: OperationId::generate().to_string(),
                generation: 1,
                supervisor_epoch: self.config.epoch,
                claim_revision: 1,
                claim_expires_unix_ms: guardian::wall_ms() + 120000,
            }),
            image_digest: self.config.images.keys().next().unwrap().clone(),
            resources: Some(Resources {
                vcpu: 1,
                memory_mib: 128,
                disk_mib: 64,
            }),
            allocation_expires_unix_ms: guardian::wall_ms() + 60000,
        }
    }
    fn manifest(&self, o: &Ownership) -> Manifest {
        guardian::read_json(
            &self
                .config
                .state_root
                .join("a")
                .join(&o.allocation_id)
                .join("manifest.json"),
        )
        .unwrap()
    }
    async fn ready(
        &self,
        c: &mut SupervisorClient<Channel>,
        o: &Ownership,
    ) -> sandbox_protocol::supervisor::Observation {
        let until = Instant::now() + Duration::from_secs(15);
        loop {
            if let Ok(r) = c
                .inspect(InspectRequest {
                    ownership: Some(o.clone()),
                })
                .await
            {
                let r = r.into_inner();
                assert_ne!(
                    r.state,
                    AllocationState::Released as i32,
                    "allocation stopped before readiness"
                );
                if r.state == AllocationState::Ready as i32 {
                    assert!(!r.simulated);
                    return r;
                }
            }
            assert!(Instant::now() < until, "guest never ready through RPC");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
    async fn released(
        &self,
        c: &mut SupervisorClient<Channel>,
        o: &Ownership,
    ) -> sandbox_protocol::supervisor::Observation {
        let until = Instant::now() + Duration::from_secs(10);
        loop {
            if let Ok(r) = c
                .inspect(InspectRequest {
                    ownership: Some(o.clone()),
                })
                .await
                && r.get_ref().state == AllocationState::Released as i32
            {
                let m = self.manifest(o);
                assert!(!m.group().exists());
                assert!(!m.directory().join("run").exists());
                assert!(m.receipt().unwrap().cleanup_confirmed);
                return r.into_inner();
            }
            assert!(
                Instant::now() < until,
                "cleanup never confirmed through RPC"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
    async fn restart(&mut self) {
        self.child.kill().unwrap();
        self.child.wait().unwrap();
        assert!(
            sandbox_supervisor::host::Host::open(self.config.clone()).is_err(),
            "same epoch cannot restart"
        );
        self.config.epoch += 1;
        fs::write(&self.config_path, serde_json::to_vec(&self.config).unwrap()).unwrap();
        self.child = Self::spawn(&self.vm, &self.config_path, &self.address, &self.tls);
        self.client().await;
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let root = self.config.state_root.join("a");
        if let Ok(entries) = fs::read_dir(root) {
            for entry in entries.flatten() {
                if let Ok(m) = guardian::read_json::<Manifest>(&entry.path().join("manifest.json"))
                {
                    let _ = guardian::control(&m, Action::Stop);
                    let until = Instant::now() + Duration::from_secs(8);
                    while m.fence_unstarted().is_err() {
                        if Instant::now() > until {
                            self.vm.temp.disable_cleanup(true);
                            return;
                        }
                        std::thread::sleep(Duration::from_millis(20));
                    }
                } else {
                    self.vm.temp.disable_cleanup(true);
                    return;
                }
            }
        }
    }
}
fn lease(o: &Ownership, revision: i64) -> LeaseOwnership {
    LeaseOwnership {
        host_id: o.host_id.clone(),
        project_id: o.project_id.clone(),
        sandbox_id: o.sandbox_id.clone(),
        allocation_id: o.allocation_id.clone(),
        generation: o.generation,
        supervisor_epoch: o.supervisor_epoch,
        revision,
        claim_expires_unix_ms: guardian::wall_ms() + 60000,
    }
}
#[tokio::test]
#[ignore = "requires root, HUDSON_GUARDIAN_TEST_VM=1 and aarch64 KVM artifacts"]
async fn real_rpc_duplicate_create_lease_stop_and_fences() {
    let f = Fixture::new().await;
    let mut c = f.client().await;
    let req = f.request();
    let o = req.ownership.as_ref().unwrap().clone();
    let mut concurrent = c.clone();
    let (a, b) = tokio::join!(concurrent.create(req.clone()), c.create(req.clone()));
    for reply in [a, b].into_iter().flatten() {
        assert!(!reply.get_ref().simulated);
    }
    let ready = f.ready(&mut c, &o).await;
    assert_eq!(ready.start_count, 1);
    assert_eq!(ready.create_operation_id, o.operation_id);
    let m = f.manifest(&o);
    let before = m.receipt().unwrap();
    assert!(before.guest_boot_id.is_some());
    // Readiness comes from a functioning authenticated guest, not process presence.
    let guest = m.guest_client().unwrap();
    assert_eq!(
        guest.hello().await.unwrap().boot_id,
        before.guest_boot_id.unwrap()
    );
    let mut changed = req.clone();
    changed.resources.as_mut().unwrap().vcpu = 2;
    assert_eq!(
        c.create(changed).await.unwrap_err().code(),
        Code::AlreadyExists
    );
    let owner = lease(&o, 1);
    let expiry = guardian::wall_ms() + 90000;
    let renewed = c
        .renew_lease(LeaseRequest {
            ownership: Some(owner.clone()),
            allocation_expires_unix_ms: expiry,
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(renewed.allocation_expires_unix_ms, expiry);
    assert!(!renewed.simulated);
    assert_eq!(
        c.renew_lease(LeaseRequest {
            ownership: Some(owner.clone()),
            allocation_expires_unix_ms: expiry + 1
        })
        .await
        .unwrap_err()
        .code(),
        Code::AlreadyExists
    );
    assert_eq!(
        c.inspect_lease(LeaseInspection {
            ownership: Some(owner)
        })
        .await
        .unwrap()
        .get_ref()
        .allocation_expires_unix_ms,
        expiry
    );
    let mut inspect = o.clone();
    inspect.claim_revision = 2;
    c.inspect(InspectRequest {
        ownership: Some(inspect.clone()),
    })
    .await
    .unwrap();
    assert_eq!(
        c.create(req.clone()).await.unwrap_err().code(),
        Code::FailedPrecondition
    );
    let _ = c
        .stop(StopRequest {
            ownership: Some(inspect.clone()),
        })
        .await;
    f.released(&mut c, &inspect).await;
    let mut retry = req;
    retry.ownership = Some(inspect);
    assert_eq!(
        c.create(retry).await.unwrap().get_ref().state,
        AllocationState::Released as i32
    );
    let absent = f.request();
    let absent_owner = absent.ownership.clone();
    let unknown_id: AllocationId = absent_owner
        .as_ref()
        .unwrap()
        .allocation_id
        .parse()
        .unwrap();
    let unknown_group = f.config.cgroup_parent.join(unknown_id.uuid().to_string());
    fs::create_dir(&unknown_group).unwrap();
    let refused = c
        .stop(StopRequest {
            ownership: absent_owner.clone(),
        })
        .await;
    fs::remove_dir(&unknown_group).unwrap();
    assert_eq!(
        refused.unwrap_err().code(),
        Code::Unavailable,
        "unknown cgroup cannot supply fenced absence"
    );
    assert_eq!(
        c.stop(StopRequest {
            ownership: absent_owner.clone()
        })
        .await
        .unwrap()
        .get_ref()
        .state,
        AllocationState::FencedAbsent as i32
    );
    assert_eq!(
        c.create(absent).await.unwrap_err().code(),
        Code::FailedPrecondition
    );
    assert_eq!(
        c.inspect(InspectRequest {
            ownership: absent_owner
        })
        .await
        .unwrap()
        .get_ref()
        .state,
        AllocationState::FencedAbsent as i32
    );
}
#[tokio::test]
#[ignore = "requires root, HUDSON_GUARDIAN_TEST_VM=1 and aarch64 KVM artifacts"]
async fn restart_advances_epoch_stops_old_owner_and_preserves_capacity() {
    let mut f = Fixture::new().await;
    let mut c = f.client().await;
    let req = f.request();
    let o = req.ownership.as_ref().unwrap().clone();
    let _ = c.create(req.clone()).await;
    f.ready(&mut c, &o).await;
    let m = f.manifest(&o);
    assert!(
        sandbox_supervisor::host::Host::open(f.config.clone()).is_err(),
        "duplicate service must not own journal"
    );
    let mut command_owner = o.clone();
    command_owner.operation_id = OperationId::generate().to_string();
    let command = sandbox_protocol::guest_model::Execute {
        operation_id: command_owner.operation_id.parse().unwrap(),
        argv: vec!["/bin/busybox".into(), "sleep".into(), "20".into()],
        env: BTreeMap::new(),
        cwd: "/".into(),
        deadline_unix_ms: guardian::wall_ms() + 25000,
        output_limit: 1024,
    };
    let command_request = sandbox_protocol::supervisor::CommandRequest {
        ownership: Some(command_owner.clone()),
        command: Some((&command).into()),
    };
    let command_reply = c
        .execute_command(command_request.clone())
        .await
        .unwrap()
        .into_inner();
    assert!(!command_reply.not_started);
    assert_eq!(
        command_reply.receipt.unwrap().state,
        sandbox_protocol::guest::State::LaunchIntent as i32
    );
    f.restart().await;
    let mut c = f.client().await;
    assert_eq!(
        c.execute_command(command_request).await.unwrap_err().code(),
        Code::FailedPrecondition
    );
    let retained: serde_json::Value =
        guardian::read_json(&f.config.state_root.join("host.json")).unwrap();
    let command_record =
        &retained["records"][&o.allocation_id]["commands"][&command_owner.operation_id];
    assert_eq!(
        command_record["digest"],
        serde_json::json!(command.digest().unwrap())
    );
    assert_eq!(command_record["not_started"], false);
    assert_eq!(command_record["receipt"]["state"], "launch_intent");
    assert_eq!(
        c.create(req.clone()).await.unwrap_err().code(),
        Code::FailedPrecondition
    );
    let mut new_epoch = req;
    new_epoch.ownership.as_mut().unwrap().supervisor_epoch = f.config.epoch;
    assert_eq!(
        c.create(new_epoch).await.unwrap_err().code(),
        Code::FailedPrecondition
    );
    let until = Instant::now() + Duration::from_secs(10);
    while !m.receipt().unwrap().cleanup_confirmed {
        assert!(Instant::now() < until);
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(!m.group().exists());
    assert!(!m.directory().join("run").exists());
    let next = f.request();
    let _ = c.create(next.clone()).await;
    f.ready(&mut c, next.ownership.as_ref().unwrap()).await;
}
#[tokio::test]
#[ignore = "requires root, HUDSON_GUARDIAN_TEST_VM=1 and aarch64 KVM artifacts"]
async fn lost_create_caller_reconciles_one_vm_and_rejects_other_controller() {
    let f = Fixture::new().await;
    let mut c = f.client().await;
    let request = f.request();
    let o = request.ownership.as_ref().unwrap().clone();
    let mut caller = c.clone();
    let call = tokio::spawn(async move { caller.create(request).await });
    let until = Instant::now() + Duration::from_secs(5);
    while !f
        .config
        .state_root
        .join("a")
        .join(&o.allocation_id)
        .join("receipt.json")
        .exists()
    {
        assert!(Instant::now() < until);
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    call.abort();
    let _ = call.await;
    assert_eq!(f.ready(&mut c, &o).await.start_count, 1);
    let rogue = tls::Leaf::new(&f.tls.ca, "host.sandbox.internal".into(), true);
    let mut rogue = transport::connect(
        &f.url,
        f.config.host,
        f.tls.ca.pem().as_bytes(),
        rogue.cert.pem().as_bytes(),
        rogue.key.serialize_pem().as_bytes(),
    )
    .await
    .unwrap();
    assert_eq!(
        rogue.health(HealthRequest {}).await.unwrap_err().code(),
        Code::PermissionDenied
    );
}

#[tokio::test]
#[ignore = "requires root, HUDSON_GUARDIAN_TEST_VM=1 and aarch64 KVM artifacts"]
async fn capacity_remains_reserved_until_real_cleanup() {
    let f = Fixture::new().await;
    let mut c = f.client().await;
    let first = f.request();
    let second = f.request();
    let third = f.request();
    for r in [&first, &second] {
        let _ = c.create(r.clone()).await;
        f.ready(&mut c, r.ownership.as_ref().unwrap()).await;
    }
    assert_eq!(
        c.create(third.clone()).await.unwrap_err().code(),
        Code::ResourceExhausted
    );
    let o = first.ownership.unwrap();
    let _ = c
        .stop(StopRequest {
            ownership: Some(o.clone()),
        })
        .await;
    f.released(&mut c, &o).await;
    let _ = c.create(third.clone()).await;
    f.ready(&mut c, third.ownership.as_ref().unwrap()).await;
    assert!(
        f.manifest(second.ownership.as_ref().unwrap())
            .group()
            .exists()
    );
}
#[tokio::test]
#[ignore = "requires root, HUDSON_GUARDIAN_TEST_VM=1 and aarch64 KVM artifacts"]
async fn running_vmm_without_guest_handshake_never_becomes_ready() {
    let f = Fixture::with_agent(false).await;
    let mut c = f.client().await;
    let mut r = f.request();
    r.allocation_expires_unix_ms = guardian::wall_ms() + 12000;
    let o = r.ownership.as_ref().unwrap().clone();
    let result = c.create(r).await;
    if let Ok(reply) = result {
        assert_ne!(reply.get_ref().state, AllocationState::Ready as i32);
    }
    let until = Instant::now() + Duration::from_secs(5);
    while Instant::now() < until {
        if let Ok(reply) = c
            .inspect(InspectRequest {
                ownership: Some(o.clone()),
            })
            .await
        {
            assert_ne!(reply.get_ref().state, AllocationState::Ready as i32);
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let _ = c
        .stop(StopRequest {
            ownership: Some(o.clone()),
        })
        .await;
    f.released(&mut c, &o).await;
}

async fn http(
    app: &axum::Router,
    token: &str,
    method: &str,
    path: &str,
    key: &str,
    body: serde_json::Value,
) -> (http::StatusCode, serde_json::Value) {
    use tower::ServiceExt;
    let request = http::Request::builder()
        .method(method)
        .uri(path)
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/json")
        .header("idempotency-key", key)
        .body(axum::body::Body::from(body.to_string()))
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 65536)
        .await
        .unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}
#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
#[ignore = "requires root, HUDSON_GUARDIAN_TEST_VM=1, local PostgreSQL and aarch64 KVM artifacts"]
async fn authenticated_api_controller_creates_renews_and_destroys_real_vm(pool: sqlx::PgPool) {
    use sandbox_protocol::{ProjectId, ProjectToken};
    use serde_json::{Value, json};
    let f = Fixture::new().await;
    let project = ProjectId::generate();
    let token = ProjectToken::generate().unwrap();
    sqlx::query("INSERT INTO projects(id,name,status,limits,api_tokens) VALUES($1,'real-vm-test','active','{}',$2)")
        .bind(project.uuid()).bind(json!([{"key_id":token.key_id().as_str(),"hash":hex::encode(token.hash().as_bytes())}])).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO hosts(id,status,cpu_capacity,memory_capacity_mib,disk_capacity_mib,supervisor_epoch) VALUES($1,'ready',2,256,128,1)")
        .bind(f.config.host.uuid()).execute(&pool).await.unwrap();
    let store = sandbox_store::Store::from_pool(pool.clone());
    let image = f.config.images.keys().next().unwrap().clone();
    let app = sandbox_api::router(sandbox_api::AppState {
        store: store.clone(),
        images: sandbox_protocol::images::ImageAllowlist::new([image.clone()]).unwrap(),
    });
    let mut controller = sandbox_controller::Controller::connect(
        store,
        sandbox_controller::ControllerConfig {
            endpoint: f.url.clone(),
            host: f.config.host,
            epoch: 1,
            allowed_images: std::collections::BTreeSet::from([image.clone()]),
            allow_simulated: false,
        },
        f.tls.ca.pem().as_bytes(),
        f.tls.host.cert.pem().as_bytes(),
        f.tls.host.key.serialize_pem().as_bytes(),
    )
    .await
    .unwrap();
    let token = token.render_once();
    let key = OperationId::generate().to_string();
    let body = json!({"image_digest":image,"resources":{"vcpu":1,"memory_mib":128,"disk_mib":64}});
    let (status, admission) = http(&app, &token, "POST", "/v1/sandboxes", &key, body.clone()).await;
    assert_eq!(status, http::StatusCode::ACCEPTED);
    let (_, repeated) = http(&app, &token, "POST", "/v1/sandboxes", &key, body).await;
    assert_eq!(repeated, admission);
    let operation = admission["operation_id"].as_str().unwrap();
    let sandbox = admission["sandbox_id"].as_str().unwrap();
    let until = Instant::now() + Duration::from_secs(25);
    loop {
        controller.tick().await.unwrap();
        let (_, result) = http(
            &app,
            &token,
            "GET",
            &format!("/v1/operations/{operation}"),
            &key,
            Value::Null,
        )
        .await;
        if result["status"] == "succeeded" {
            assert_eq!(result["result"]["simulated"], false);
            break;
        }
        assert!(
            Instant::now() < until,
            "create failed to converge: {result}"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let (_, state) = http(
        &app,
        &token,
        "GET",
        &format!("/v1/sandboxes/{sandbox}"),
        &key,
        Value::Null,
    )
    .await;
    assert_eq!(state["observed_state"], "running");
    assert_eq!(state["observation_simulated"], false);
    let (allocation, initial): (String, i64) = sqlx::query_as(
        "SELECT id::text,(extract(epoch FROM lease_expires_at)*1000)::bigint FROM allocations",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    let m: Manifest = guardian::read_json(
        &f.config
            .state_root
            .join("a")
            .join(format!("{}_{allocation}", AllocationId::PREFIX))
            .join("manifest.json"),
    )
    .unwrap();
    assert!(m.receipt().unwrap().guest_boot_id.is_some());
    assert_eq!(
        m.guest_client()
            .unwrap()
            .hello()
            .await
            .unwrap()
            .allocation_id,
        m.start.owner.allocation
    );
    let until = Instant::now() + Duration::from_secs(20);
    loop {
        controller.tick().await.unwrap();
        let (renewed,):(bool,)=sqlx::query_as("SELECT (extract(epoch FROM lease_expires_at)*1000)::bigint>$1 AND maintenance_revision>0 AND NOT renewal_pending FROM allocations").bind(initial).fetch_one(&pool).await.unwrap();
        if renewed {
            break;
        }
        assert!(
            Instant::now() < until,
            "controller never renewed execution lease"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    // Authenticated public execute is owned by the runtime after admission.
    for (argv, expected_status, expected_exit) in [
        (
            vec![
                "/bin/busybox",
                "sh",
                "-c",
                "echo public-result; /bin/busybox sleep 1; exit 0",
            ],
            "succeeded",
            Some(0),
        ),
        (
            vec!["/bin/busybox", "sh", "-c", "exit 7"],
            "failed",
            Some(7),
        ),
        (vec!["/bin/busybox", "sleep", "10"], "failed", None),
    ] {
        let execute_key = OperationId::generate().to_string();
        let command = json!({"argv":argv,"deadline_unix_ms":guardian::wall_ms()+if expected_exit.is_none(){1500}else{10000},"output_limit":1024});
        let (status, admitted) = http(
            &app,
            &token,
            "POST",
            &format!("/v1/sandboxes/{sandbox}/execute"),
            &execute_key,
            command.clone(),
        )
        .await;
        assert_eq!(status, http::StatusCode::ACCEPTED, "{admitted}");
        let (_, retry) = http(
            &app,
            &token,
            "POST",
            &format!("/v1/sandboxes/{sandbox}/execute"),
            &execute_key,
            command,
        )
        .await;
        assert_eq!(admitted, retry);
        let id = admitted["operation_id"].as_str().unwrap();
        let until = Instant::now() + Duration::from_secs(15);
        loop {
            controller.tick().await.unwrap();
            let (_, result) = http(
                &app,
                &token,
                "GET",
                &format!("/v1/operations/{id}"),
                &execute_key,
                Value::Null,
            )
            .await;
            if result["status"] == expected_status {
                assert_eq!(result["result"]["simulated"], false);
                assert_eq!(result["result"]["exit_code"], json!(expected_exit));
                if expected_exit.is_none() {
                    assert_eq!(result["phase"], "timed_out");
                }
                break;
            }
            assert!(
                Instant::now() < until,
                "execute failed to converge: {result}"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
    let (status, destroy) = http(
        &app,
        &token,
        "POST",
        &format!("/v1/sandboxes/{sandbox}/destroy"),
        &OperationId::generate().to_string(),
        json!({}),
    )
    .await;
    assert_eq!(status, http::StatusCode::ACCEPTED, "{destroy}");
    let destroy = destroy["operation_id"].as_str().unwrap();
    let until = Instant::now() + Duration::from_secs(15);
    loop {
        controller.tick().await.unwrap();
        let (_, result) = http(
            &app,
            &token,
            "GET",
            &format!("/v1/operations/{destroy}"),
            &key,
            Value::Null,
        )
        .await;
        if result["status"] == "succeeded" {
            break;
        }
        assert!(
            Instant::now() < until,
            "destroy failed to converge: {result}"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let (_, state) = http(
        &app,
        &token,
        "GET",
        &format!("/v1/sandboxes/{sandbox}"),
        &key,
        Value::Null,
    )
    .await;
    assert_eq!(state["observed_state"], "destroyed");
    assert_eq!(state["observation_simulated"], false);
    assert!(!m.group().exists());
    assert!(!m.directory().join("run").exists());
    assert!(m.receipt().unwrap().cleanup_confirmed);
    let (released,):(bool,)=sqlx::query_as("SELECT released_at IS NOT NULL AND release_evidence->>'simulated'='false' FROM allocations").fetch_one(&pool).await.unwrap();
    assert!(released);
    eprintln!(
        "real_api_lifecycle_observation {}",
        json!({"create_succeeded":true,"duplicate_handles_match":true,"simulated":false,"guest_boot_bound":true,"lease_renewed":true,"destroy_succeeded":true,"database_release_confirmed":true,"cgroup_removed":true,"runtime_files_removed":true})
    );
}

#[tokio::test]
#[ignore = "requires root, HUDSON_GUARDIAN_TEST_VM=1 and aarch64 KVM artifacts"]
async fn real_command_rpc_retains_results_fences_absence_and_survives_request_end() {
    use sandbox_protocol::{
        guest_model as m,
        supervisor::{CommandInspection, CommandRequest},
    };
    let f = Fixture::new().await;
    let mut c = f.client().await;
    let request = f.request();
    let allocation = request.ownership.as_ref().unwrap().clone();
    let _ = c.create(request).await;
    f.ready(&mut c, &allocation).await;
    let mut owner = allocation.clone();
    owner.operation_id = OperationId::generate().to_string();
    let command = m::Execute {
        operation_id: owner.operation_id.parse().unwrap(),
        argv: vec![
            "/bin/busybox".into(),
            "sh".into(),
            "-c".into(),
            "echo once >> /execution-marker; /bin/busybox sleep 1; exit 7".into(),
        ],
        env: BTreeMap::new(),
        cwd: "/".into(),
        deadline_unix_ms: guardian::wall_ms() + 15000,
        output_limit: 1024,
    };
    let request = CommandRequest {
        ownership: Some(owner.clone()),
        command: Some((&command).into()),
    };
    let reply = c
        .execute_command(request.clone())
        .await
        .unwrap()
        .into_inner();
    assert!(!reply.simulated);
    assert!(!reply.not_started);
    // The command continues after this RPC response. Retry observes the same intent.
    let _ = c.execute_command(request.clone()).await.unwrap();
    let until = Instant::now() + Duration::from_secs(10);
    loop {
        let r = c
            .inspect_command(CommandInspection {
                ownership: Some(owner.clone()),
                command_digest: command.digest().unwrap().to_vec(),
            })
            .await
            .unwrap()
            .into_inner();
        if let Some(receipt) = r.receipt {
            let receipt: m::Receipt = receipt.try_into().unwrap();
            if receipt.state == m::State::Exited {
                assert_eq!(receipt.exit_code, Some(7));
                assert!(receipt.cleanup_confirmed);
                break;
            }
        }
        assert!(Instant::now() < until, "command did not finish");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let replay = c
        .execute_command(request.clone())
        .await
        .unwrap()
        .into_inner();
    assert_eq!(replay.receipt.unwrap().exit_code, Some(7));
    let mut changed = request;
    changed
        .command
        .as_mut()
        .unwrap()
        .argv
        .push("changed".into());
    assert_eq!(
        c.execute_command(changed).await.unwrap_err().code(),
        Code::AlreadyExists
    );
    // Private output read verifies the side effect occurred once; public output is separate work.
    let client = f.manifest(&allocation).guest_client().unwrap();
    let verify = m::Execute {
        operation_id: OperationId::generate(),
        argv: vec![
            "/bin/busybox".into(),
            "cat".into(),
            "/execution-marker".into(),
        ],
        env: BTreeMap::new(),
        cwd: "/".into(),
        deadline_unix_ms: guardian::wall_ms() + 5000,
        output_limit: 1024,
    };
    client.execute(&verify).await.unwrap();
    let until = Instant::now() + Duration::from_secs(5);
    while !client
        .inspect(verify.operation_id)
        .await
        .unwrap()
        .cleanup_confirmed
    {
        assert!(Instant::now() < until);
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let output = client
        .output(sandbox_protocol::guest::ReadOutput {
            operation_id: verify.operation_id.to_string(),
            stream: sandbox_protocol::guest::Stream::Stdout as i32,
            offset: 0,
            limit: 1024,
        })
        .await
        .unwrap();
    assert_eq!(output.data, b"once\n");
    let mut absent_owner = owner.clone();
    absent_owner.operation_id = OperationId::generate().to_string();
    let mut absent = command.clone();
    absent.operation_id = absent_owner.operation_id.parse().unwrap();
    assert!(
        c.inspect_command(CommandInspection {
            ownership: Some(absent_owner.clone()),
            command_digest: absent.digest().unwrap().to_vec()
        })
        .await
        .unwrap()
        .into_inner()
        .not_started
    );
    assert!(
        c.execute_command(CommandRequest {
            ownership: Some(absent_owner),
            command: Some((&absent).into())
        })
        .await
        .unwrap()
        .into_inner()
        .not_started
    );
    let mut active_owner = owner.clone();
    active_owner.operation_id = OperationId::generate().to_string();
    let mut active = command.clone();
    active.operation_id = active_owner.operation_id.parse().unwrap();
    active.argv = vec!["/bin/busybox".into(), "sleep".into(), "20".into()];
    active.deadline_unix_ms = guardian::wall_ms() + 25000;
    let response = c
        .execute_command(CommandRequest {
            ownership: Some(active_owner.clone()),
            command: Some((&active).into()),
        })
        .await
        .unwrap()
        .into_inner();
    assert!(!response.not_started);
    let mut stop = allocation.clone();
    stop.operation_id = OperationId::generate().to_string();
    let _ = c
        .stop(StopRequest {
            ownership: Some(stop.clone()),
        })
        .await;
    f.released(&mut c, &stop).await;
    let uncertain = c
        .inspect_command(CommandInspection {
            ownership: Some(active_owner),
            command_digest: active.digest().unwrap().to_vec(),
        })
        .await
        .unwrap()
        .into_inner();
    assert!(!uncertain.not_started);
    assert!(uncertain.receipt.is_none());
    // A previously retained exit survives cleanup; the active command's result does not get invented.
    let completed = c
        .inspect_command(CommandInspection {
            ownership: Some(owner),
            command_digest: command.digest().unwrap().to_vec(),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(completed.receipt.unwrap().exit_code, Some(7));
}
