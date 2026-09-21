//! Real PostgreSQL transactions with synthetic guest receipts, not VM evidence.
#![allow(clippy::unwrap_used, clippy::expect_used, dead_code)]
use sandbox_protocol::{
    AllocationId, HostId, Id, IdempotencyKey, OperationId, ProjectId, ProjectToken, SandboxId,
    guest_model::{self as guest, Receipt, State},
    output::{OutputName, OutputPlan, OutputPlans, OutputRef, OutputRefs},
    supervisor::CommandObservation,
};
use sandbox_store::{
    Store,
    claims::{Claim, OperationKind},
    execute::{ExecuteAction, ExecuteAdmission, ExecuteCommand},
    output::{OutputClaim, OutputWork},
};
use serde_json::{Value, json};
use sqlx::PgPool;
use time::OffsetDateTime;

fn now() -> i64 {
    (OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000) as i64
}
pub(super) struct Fixture {
    pub(super) token: String,
    pub(super) store: Store,
    pub(super) project: ProjectId,
    sandbox: SandboxId,
    allocation: AllocationId,
    host: HostId,
    pub(super) operation: OperationId,
    execution_claim: Claim,
    observation: CommandObservation,
}
impl Fixture {
    pub(super) async fn new(pool: &PgPool) -> Self {
        let project = ProjectId::generate();
        let sandbox = SandboxId::generate();
        let allocation = AllocationId::generate();
        let host = HostId::generate();
        let token = ProjectToken::generate().unwrap();
        sqlx::query("INSERT INTO projects(id,name,status,limits,api_tokens) VALUES($1,'output-test','active','{}',$2)")
            .bind(project.uuid()).bind(json!([{"key_id":token.key_id().as_str(),"hash":hex::encode(token.hash().as_bytes())}])).execute(pool).await.unwrap();
        sqlx::query("INSERT INTO hosts(id,status,cpu_capacity,memory_capacity_mib,disk_capacity_mib,supervisor_epoch,last_seen_at) VALUES($1,'ready',4,8192,65536,1,clock_timestamp())")
            .bind(host.uuid()).execute(pool).await.unwrap();
        sqlx::query("INSERT INTO sandboxes(id,project_id,image_digest,resources,desired_state,observed_state,generation) VALUES($1,$2,'sha256:fixture','{}','running','running',1)")
            .bind(sandbox.uuid()).bind(project.uuid()).execute(pool).await.unwrap();
        sqlx::query("INSERT INTO allocations(id,project_id,sandbox_id,host_id,generation,supervisor_epoch,vcpu,memory_mib,disk_mib,status,lease_expires_at) VALUES($1,$2,$3,$4,1,1,1,128,64,'running',clock_timestamp()+interval '5 minutes')")
            .bind(allocation.uuid()).bind(project.uuid()).bind(sandbox.uuid()).bind(host.uuid()).execute(pool).await.unwrap();
        sqlx::query("UPDATE sandboxes SET current_allocation_id=$2 WHERE id=$1")
            .bind(sandbox.uuid())
            .bind(allocation.uuid())
            .execute(pool)
            .await
            .unwrap();
        let store = Store::from_pool(pool.clone());
        let request=ExecuteCommand { project_id:project,sandbox_id:sandbox,key_id:token.key_id().clone(),
            idempotency_key: IdempotencyKey::parse(&OperationId::generate().to_string()).unwrap(),
            command: serde_json::from_value(json!({"argv":["private-command"],"deadline_unix_ms":now()+60000,"output_limit":100})).unwrap() };
        let ExecuteAdmission::Accepted {
            operation_id: operation,
            ..
        } = store.admit_execute(&request).await.unwrap()
        else {
            panic!("admit")
        };
        let execution_claim = store
            .claim_next(OperationKind::Execute, 30)
            .await
            .unwrap()
            .unwrap();
        let ExecuteAction::Dispatch { owner, command } = store
            .prepare_execute(&execution_claim, host, 1)
            .await
            .unwrap()
        else {
            panic!("dispatch")
        };
        let digest = command.digest().unwrap();
        let receipt = Receipt {
            version: 1,
            context: guest::Context {
                allocation_id: allocation,
                generation: 1,
                boot_id: "pinned-guest-boot".into(),
            },
            operation_id: operation,
            digest,
            state: State::Exited,
            deadline_unix_ms: command.deadline_unix_ms,
            output_limit: command.output_limit,
            cancel_requested: false,
            cleanup_confirmed: true,
            exit_code: Some(0),
            signal: None,
            stdout: guest::Output {
                seen: 10,
                stored: 4,
                truncated: true,
            },
            stderr: guest::Output {
                seen: 3,
                stored: 3,
                truncated: false,
            },
            reason: None,
        };
        let observation = CommandObservation {
            ownership: Some(owner),
            simulated: true,
            observed_unix_ms: now(),
            command_digest: digest.to_vec(),
            receipt: Some((&receipt).into()),
            not_started: false,
        };
        Self {
            token: token.render_once(),
            store,
            project,
            sandbox,
            allocation,
            host,
            operation,
            execution_claim,
            observation,
        }
    }
    pub(super) async fn finish(&self) {
        self.store
            .record_execute_observation(&self.execution_claim, &self.observation, true)
            .await
            .unwrap();
    }
    pub(super) async fn work(&self) -> (OutputClaim, OutputWork) {
        let claim = self.store.claim_output(30).await.unwrap().unwrap();
        let work = self
            .store
            .prepare_output(&claim, 3600, 600, true)
            .await
            .unwrap();
        (claim, work)
    }
    pub(super) async fn snapshot(&self) -> Value {
        sqlx::query_scalar("SELECT jsonb_build_array(status,phase,result,error,attempt_count,attempt_receipts,claim_revision,
            lease_expires_at,next_retry_at,completed_at,execution_allocation_id) FROM operations WHERE id=$1")
            .bind(self.operation.uuid()).fetch_one(self.store.pool()).await.unwrap()
    }
    async fn expire_claim(&self) {
        sqlx::query("UPDATE operations SET output_lease_expires_at=clock_timestamp()-interval '1 second' WHERE id=$1")
            .bind(self.operation.uuid()).execute(self.store.pool()).await.unwrap();
    }
}
pub(super) fn plans(work: &OutputWork) -> OutputPlans {
    let plan = |name, stats: &guest::Output| OutputPlan {
        version: 1,
        owner: work.ticket.owner.clone(),
        upload_attempt: work.ticket.upload_attempt,
        name,
        size: stats.stored,
        sha256: {
            use sha2::Digest;
            hex::encode(sha2::Sha256::digest(match name {
                OutputName::Stdout => b"a\x00b\xff".as_slice(),
                OutputName::Stderr => b"err".as_slice(),
            }))
        },
        seen: stats.seen,
        truncated: stats.truncated,
        created_unix_ms: work.ticket.created_unix_ms,
        expires_unix_ms: work.ticket.expires_unix_ms,
        delete_after_unix_ms: work.ticket.delete_after_unix_ms,
    };
    OutputPlans {
        stdout: plan(OutputName::Stdout, &work.receipt.stdout),
        stderr: plan(OutputName::Stderr, &work.receipt.stderr),
    }
}
pub(super) fn refs(plans: &OutputPlans) -> OutputRefs {
    let reference = |plan: &OutputPlan| OutputRef {
        plan: plan.clone(),
        etag: "test-etag".into(),
        object_version: Some("test-version".into()),
    };
    OutputRefs {
        stdout: reference(&plans.stdout),
        stderr: reference(&plans.stderr),
    }
}

impl Fixture {
    pub(super) async fn publish(&self) {
        self.finish().await;
        let (claim, work) = self.work().await;
        let plans = plans(&work);
        self.store
            .save_output_plans(&claim, &plans, true)
            .await
            .unwrap();
        self.store
            .publish_output(&claim, &refs(&plans), true)
            .await
            .unwrap();
    }
    pub(super) fn app(
        &self,
        reader: Option<std::sync::Arc<dyn sandbox_api::outputs::OutputReader>>,
    ) -> axum::Router {
        sandbox_api::router_with_output(
            sandbox_api::AppState {
                store: self.store.clone(),
                images: sandbox_protocol::images::ImageAllowlist::new([format!(
                    "sha256:{}",
                    "a".repeat(64)
                )])
                .unwrap(),
            },
            reader,
        )
    }
}
