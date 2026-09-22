use super::*;
use sandbox_artifacts::{Error, sources::SourceStore};
use sandbox_cleanup::sources::{SourceCleaner, SourceCleanupTick, SourceRetirementBackend};
use sandbox_protocol::file_sources::{SourceOwner, SourcePlan, SourceRef, SourceRetirement};
use sandbox_store::dispatch::DispatchError;

async fn aged(f: &Fixture, s: SandboxId, bytes: Vec<u8>) -> (OperationId, SourcePlan) {
    let (_, a) = admit(f, s, bytes).await;
    let id: OperationId = a["operation_id"].as_str().unwrap().parse().unwrap();
    let raw: Value = sqlx::query_scalar("SELECT plan FROM file_uploads WHERE operation_id=$1")
        .bind(id.uuid())
        .fetch_one(f.store.pool())
        .await
        .unwrap();
    let mut plan: SourcePlan = serde_json::from_value(raw).unwrap();
    for t in [
        &mut plan.created_unix_ms,
        &mut plan.write_expires_unix_ms,
        &mut plan.expires_unix_ms,
        &mut plan.delete_after_unix_ms,
    ] {
        *t -= 7200000;
    }
    plan.validate().unwrap();
    sqlx::query("UPDATE file_uploads SET plan=$2 WHERE operation_id=$1")
        .bind(id.uuid())
        .bind(serde_json::json!(plan))
        .execute(f.store.pool())
        .await
        .unwrap();
    sqlx::query("UPDATE operations SET deadline=clock_timestamp()-interval '1 hour' WHERE id=$1")
        .bind(id.uuid())
        .execute(f.store.pool())
        .await
        .unwrap();
    (id, plan)
}
fn receipt(plan: &SourcePlan, selected: Option<&SourceRef>) -> SourceRetirement {
    SourceRetirement {
        version: 1,
        plan_sha256: plan.metadata_digest().unwrap(),
        previous: selected.cloned(),
        marker_etag: "retired".into(),
        marker_version: None,
    }
}
async fn snapshot(f: &Fixture, id: OperationId) -> Value {
    sqlx::query_scalar("SELECT to_jsonb(o) FROM operations o WHERE id=$1")
        .bind(id.uuid())
        .fetch_one(f.store.pool())
        .await
        .unwrap()
}
async fn unretired_bytes(f: &Fixture) -> i64 {
    sqlx::query_scalar("SELECT COALESCE(sum(size) FILTER(WHERE source_retired_at IS NULL),0)::bigint FROM file_uploads").fetch_one(f.store.pool()).await.unwrap()
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn freeze_and_verified_completion_preserve_outcomes_and_guest_reservations(pool: PgPool) {
    let _guard = TEST_LOCK.lock().await;
    let (f, _sources, _c, s) = setup(&pool).await;
    let (id, plan) = aged(&f, s, b"private-data".to_vec()).await;
    let before = snapshot(&f, id).await;
    // Storage cleanup uses original ownership even after customer revocation or host epoch change.
    sqlx::query("UPDATE projects SET status='suspended'")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE hosts SET supervisor_epoch=supervisor_epoch+1")
        .execute(&pool)
        .await
        .unwrap();
    let claim = f
        .store
        .claim_file_source_cleanup(30)
        .await
        .unwrap()
        .unwrap();
    assert!(
        f.store
            .claim_file_source_cleanup(30)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(unretired_bytes(&f).await, 12);
    let work = f.store.prepare_file_source_cleanup(&claim).await.unwrap();
    assert_eq!(work.manifest.plan, plan);
    assert!(work.manifest.selected.is_none());
    assert!(work.now_unix_ms >= plan.delete_after_unix_ms);
    let mut bad = receipt(&plan, None);
    bad.plan_sha256 = "f".repeat(64);
    assert!(matches!(
        f.store.complete_file_source_cleanup(&claim, &bad).await,
        Err(DispatchError::BadEvidence)
    ));
    assert_eq!(unretired_bytes(&f).await, 12);
    f.store
        .complete_file_source_cleanup(&claim, &receipt(&plan, None))
        .await
        .unwrap();
    assert_eq!(unretired_bytes(&f).await, 0);
    assert_eq!(snapshot(&f, id).await, before);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT sum(size)::bigint FROM file_uploads")
            .fetch_one(&pool)
            .await
            .unwrap(),
        12
    );
    f.store
        .complete_file_source_cleanup(&claim, &receipt(&plan, None))
        .await
        .unwrap();
    assert_eq!(unretired_bytes(&f).await, 0);
    assert!(
        f.store
            .claim_file_source_cleanup(30)
            .await
            .unwrap()
            .is_none()
    );
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn expiry_claim_grace_and_frozen_dispatch_are_independent_gates(pool: PgPool) {
    let _guard = TEST_LOCK.lock().await;
    let (f, _sources, _c, s) = setup(&pool).await;
    let (_, a) = admit(&f, s, b"data".to_vec()).await;
    assert!(
        f.store
            .claim_file_source_cleanup(30)
            .await
            .unwrap()
            .is_none()
    );
    // Age only the source. A still-valid operation deadline must prevent retirement.
    let id = a["operation_id"]
        .as_str()
        .unwrap()
        .parse::<OperationId>()
        .unwrap();
    let mut p: SourcePlan = serde_json::from_value(
        sqlx::query_scalar::<_, Value>("SELECT plan FROM file_uploads")
            .fetch_one(&pool)
            .await
            .unwrap(),
    )
    .unwrap();
    for t in [
        &mut p.created_unix_ms,
        &mut p.write_expires_unix_ms,
        &mut p.expires_unix_ms,
        &mut p.delete_after_unix_ms,
    ] {
        *t -= 7200000;
    }
    sqlx::query("UPDATE file_uploads SET plan=$1")
        .bind(serde_json::json!(p))
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        f.store
            .claim_file_source_cleanup(30)
            .await
            .unwrap()
            .is_none()
    );
    sqlx::query(
        "UPDATE operations SET deadline=clock_timestamp()-interval '4 minutes' WHERE id=$1",
    )
    .bind(id.uuid())
    .execute(&pool)
    .await
    .unwrap();
    assert!(
        f.store
            .claim_file_source_cleanup(30)
            .await
            .unwrap()
            .is_none()
    );
    sqlx::query("UPDATE operations SET deadline=clock_timestamp()-interval '6 minutes',lease_expires_at=clock_timestamp()+interval '1 minute' WHERE id=$1").bind(id.uuid()).execute(&pool).await.unwrap();
    assert!(
        f.store
            .claim_file_source_cleanup(30)
            .await
            .unwrap()
            .is_none()
    );
    f.reclaim_now().await;
    let cleanup = f
        .store
        .claim_file_source_cleanup(30)
        .await
        .unwrap()
        .unwrap();
    // Extending a deadline cannot reopen a frozen source or permit guest Begin.
    sqlx::query(
        "UPDATE operations SET deadline=clock_timestamp()+interval '5 minutes' WHERE id=$1",
    )
    .bind(id.uuid())
    .execute(&pool)
    .await
    .unwrap();
    let op = f
        .store
        .claim_next(OperationKind::FileWrite, 30)
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        f.store
            .accept_upload_source(
                &op,
                &SourceRef {
                    plan: p.clone(),
                    etag: "old".into(),
                    object_version: None
                }
            )
            .await,
        Err(DispatchError::BadEvidence)
    ));
    assert!(matches!(
        f.store.prepare_upload(&op, f.config.host, 1).await.unwrap(),
        UploadAction::Rejected
    ));
    assert_eq!(f.fake.total_file_commits().await, 0);
    assert!(matches!(
        f.store.prepare_file_source_cleanup(&cleanup).await,
        Err(DispatchError::LostClaim)
    ));
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn changed_manifest_source_or_expired_claim_cannot_refund_bytes(pool: PgPool) {
    let _guard = TEST_LOCK.lock().await;
    let (f, sources, _c, s) = setup(&pool).await;
    let (id, p) = aged(&f, s, b"data".to_vec()).await;
    let selected = SourceRef {
        plan: p.clone(),
        etag: "original".into(),
        object_version: None,
    };
    sqlx::query("UPDATE file_uploads SET source_ref=$1")
        .bind(serde_json::json!(selected))
        .execute(&pool)
        .await
        .unwrap();
    let stale = f
        .store
        .claim_file_source_cleanup(30)
        .await
        .unwrap()
        .unwrap();
    sqlx::query(
        "UPDATE file_uploads SET source_cleanup_lease_until=clock_timestamp()-interval '1 second'",
    )
    .execute(&pool)
    .await
    .unwrap();
    let live = f
        .store
        .claim_file_source_cleanup(30)
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        f.store
            .complete_file_source_cleanup(&stale, &receipt(&p, Some(&selected)))
            .await,
        Err(DispatchError::LostClaim)
    ));
    assert!(matches!(
        f.store
            .complete_file_source_cleanup(&live, &receipt(&p, None))
            .await,
        Err(DispatchError::BadEvidence)
    ));
    sqlx::query("UPDATE file_uploads SET source_ref=jsonb_set(source_ref,'{etag}','\"changed\"')")
        .execute(&pool)
        .await
        .unwrap();
    assert!(matches!(
        f.store.prepare_file_source_cleanup(&live).await,
        Err(DispatchError::InvalidData)
    ));
    sqlx::query("UPDATE file_uploads SET source_ref=$1")
        .bind(serde_json::json!(selected))
        .execute(&pool)
        .await
        .unwrap();
    let manifest: Value = sqlx::query_scalar("SELECT source_cleanup_manifest FROM file_uploads")
        .fetch_one(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE file_uploads SET source_cleanup_manifest=jsonb_set(source_cleanup_manifest,'{plan,owner,scope,generation}','99')").execute(&pool).await.unwrap();
    assert!(matches!(
        f.store.prepare_file_source_cleanup(&live).await,
        Err(DispatchError::InvalidData)
    ));
    sqlx::query("UPDATE file_uploads SET source_cleanup_manifest=$1")
        .bind(manifest)
        .execute(&pool)
        .await
        .unwrap();
    let before = snapshot(&f, id).await;
    f.store
        .complete_file_source_cleanup(&live, &receipt(&p, Some(&selected)))
        .await
        .unwrap();
    assert_eq!(snapshot(&f, id).await, before);
    assert_eq!(unretired_bytes(&f).await, 0);
    let key: String = sqlx::query_scalar("SELECT idempotency_key FROM operations WHERE id=$1")
        .bind(id.uuid())
        .fetch_one(&pool)
        .await
        .unwrap();
    let uploads = sources.uploads.load(Ordering::SeqCst);
    let (status, retry) = put(f.app.clone(), f.token.clone(), s, key, b"data".to_vec()).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(retry["operation_id"], id.to_string());
    assert_eq!(sources.uploads.load(Ordering::SeqCst), uploads);
}

struct Paused {
    started: tokio::sync::Notify,
    resume: tokio::sync::Semaphore,
}
struct PausedBackend(Arc<Paused>);
impl SourceRetirementBackend for PausedBackend {
    async fn retire(
        &self,
        p: &SourcePlan,
        _: &SourceOwner,
        s: Option<&SourceRef>,
        _: i64,
    ) -> Result<SourceRetirement, Error> {
        self.0.started.notify_one();
        self.0.resume.acquire().await.unwrap().forget();
        Ok(receipt(p, s))
    }
}
#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn cancelled_worker_reclaims_same_frozen_attempt_without_changing_operation(pool: PgPool) {
    let _guard = TEST_LOCK.lock().await;
    let (f, _sources, _c, s) = setup(&pool).await;
    let (id, p) = aged(&f, s, b"data".to_vec()).await;
    let before = snapshot(&f, id).await;
    let paused = Arc::new(Paused {
        started: tokio::sync::Notify::new(),
        resume: tokio::sync::Semaphore::new(0),
    });
    let worker = SourceCleaner::new(f.store.clone(), PausedBackend(paused.clone()));
    let task = tokio::spawn(async move { worker.tick().await });
    tokio::time::timeout(Duration::from_secs(3), paused.started.notified())
        .await
        .unwrap();
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    assert_eq!(unretired_bytes(&f).await, 4);
    sqlx::query(
        "UPDATE file_uploads SET source_cleanup_lease_until=clock_timestamp()-interval '1 second'",
    )
    .execute(&pool)
    .await
    .unwrap();
    let c = f
        .store
        .claim_file_source_cleanup(30)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        f.store
            .prepare_file_source_cleanup(&c)
            .await
            .unwrap()
            .manifest
            .plan,
        p
    );
    f.store
        .complete_file_source_cleanup(&c, &receipt(&p, None))
        .await
        .unwrap();
    assert_eq!(snapshot(&f, id).await, before);
}

fn config() -> sandbox_artifacts::S3Config {
    sandbox_artifacts::S3Config {
        endpoint: std::env::var("HUDSON_TEST_S3_ENDPOINT").unwrap(),
        region: "us-east-1".into(),
        bucket: std::env::var("HUDSON_TEST_S3_VERSIONED_BUCKET").unwrap(),
        access_key: std::env::var("HUDSON_TEST_S3_ACCESS_KEY").unwrap(),
        secret_key: std::env::var("HUDSON_TEST_S3_SECRET_KEY").unwrap(),
        session_token: None,
        allow_loopback_http: true,
    }
}
struct LoseRetirementReply {
    inner: sandbox_artifacts::sources::SourceRetirer,
    lost: std::sync::atomic::AtomicBool,
}
impl SourceRetirementBackend for LoseRetirementReply {
    async fn retire(
        &self,
        p: &SourcePlan,
        o: &SourceOwner,
        s: Option<&SourceRef>,
        now: i64,
    ) -> Result<SourceRetirement, Error> {
        let receipt = self.inner.retire(p, o, s, now).await?;
        if !self.lost.swap(true, Ordering::SeqCst) {
            return Err(Error::Unavailable);
        }
        Ok(receipt)
    }
}
#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
#[ignore = "requires private versioned MinIO bucket and HUDSON_TEST_S3_* configuration"]
async fn output_minio_file_source_cleanup_reconciles_lost_retirement_reply(pool: PgPool) {
    let _guard = TEST_LOCK.lock().await;
    let (f, _sources, _c, s) = setup(&pool).await;
    let bytes = (0..65539).map(|n| (n % 251) as u8).collect::<Vec<_>>();
    let (id, p) = aged(&f, s, bytes.clone()).await;
    let sources: SourceStore = config().build_sources().unwrap();
    let reference = sources
        .upload(&p, &p.owner, p.created_unix_ms + 1, &bytes)
        .await
        .unwrap();
    // API PUT reply was lost; no source_ref was ever selected by the controller.
    let before = snapshot(&f, id).await;
    let worker = SourceCleaner::new(
        f.store.clone(),
        LoseRetirementReply {
            inner: config().build_source_retirer().unwrap(),
            lost: std::sync::atomic::AtomicBool::new(false),
        },
    );
    assert!(worker.tick().await.is_err());
    assert_eq!(unretired_bytes(&f).await, bytes.len() as i64);
    assert!(matches!(
        sources
            .read(&reference, &p.owner, p.created_unix_ms + 1)
            .await,
        Err(Error::Missing)
    ));
    sqlx::query("UPDATE file_uploads SET source_cleanup_next_at=NULL")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        worker.tick().await.unwrap(),
        SourceCleanupTick::Completed(id)
    );
    assert_eq!(worker.tick().await.unwrap(), SourceCleanupTick::Idle);
    assert_eq!(unretired_bytes(&f).await, 0);
    assert_eq!(snapshot(&f, id).await, before);
    assert!(
        sources
            .upload(&p, &p.owner, p.created_unix_ms + 1, &bytes)
            .await
            .is_err()
    );
    let retired: Value = sqlx::query_scalar("SELECT source_retirement FROM file_uploads")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        serde_json::from_value::<SourceRetirement>(retired)
            .unwrap()
            .previous,
        Some(reference)
    );
    eprintln!(
        "file_source_cleanup_observation={}",
        serde_json::json!({"bytes":bytes.len(),"versioned_payload_removed":true,"lost_reply_reconciled":true,"late_put_rejected":true,"operation_unchanged":true,"guest_reservation_retained":true})
    );
}

#[sqlx::test(migrations = false)]
async fn upgrade_preserves_admitted_sources_and_does_not_invent_retirement(pool: PgPool) {
    let _guard = TEST_LOCK.lock().await;
    sqlx::migrate::Migrator {
        migrations: std::borrow::Cow::Owned(
            sandbox_store::MIGRATOR.iter().take(13).cloned().collect(),
        ),
        ..sqlx::migrate::Migrator::DEFAULT
    }
    .run(&pool)
    .await
    .unwrap();
    let f = Fixture::new(&pool).await;
    let (_, s) = f.admit().await;
    f.controller().await.tick().await.unwrap();
    let (project, allocation, host, generation, epoch): (
        uuid::Uuid,
        uuid::Uuid,
        uuid::Uuid,
        i64,
        i64,
    ) = sqlx::query_as("SELECT project_id,id,host_id,generation,supervisor_epoch FROM allocations")
        .fetch_one(&pool)
        .await
        .unwrap();
    let id = OperationId::generate();
    let now = (time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000) as i64;
    let p = SourcePlan {
        version: 1,
        owner: SourceOwner {
            operation_id: id,
            scope: sandbox_protocol::file_downloads::ReadScope {
                version: 1,
                project_id: sandbox_protocol::ProjectId::from_uuid(project),
                sandbox_id: s,
                allocation_id: sandbox_protocol::AllocationId::from_uuid(allocation),
                host_id: sandbox_protocol::HostId::from_uuid(host),
                generation,
                host_epoch: epoch,
            },
        },
        upload: sandbox_protocol::files::Upload {
            operation_id: id,
            path: "old.bin".into(),
            size: 4,
            sha256: Sha256::digest(b"data").into(),
            mode: 0o644,
        },
        source_attempt: OperationId::generate(),
        created_unix_ms: now - 7200000,
        write_expires_unix_ms: now - 6900000,
        expires_unix_ms: now - 3600000,
        delete_after_unix_ms: now - 3600000,
    };
    let payload = serde_json::json!({"path":p.upload.path,"size":p.upload.size,"sha256":p.upload.sha256,"mode":p.upload.mode});
    let digest = sandbox_protocol::RequestDigest::compute(
        "PUT",
        &format!("/v1/sandboxes/{s}/files"),
        &payload,
    )
    .unwrap();
    sqlx::query("INSERT INTO operations(id,project_id,sandbox_id,kind,initiator_kind,idempotency_key,request_digest,digest_version,payload,status,file_allocation_id,deadline) VALUES($1,$2,$3,'file_write','service',$4,$5,1,$6,'unknown',$7,clock_timestamp()-interval '1 hour')").bind(id.uuid()).bind(project).bind(s.uuid()).bind(id.to_string()).bind(digest.as_bytes().as_slice()).bind(payload).bind(allocation).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO file_uploads(operation_id,project_id,sandbox_id,allocation_id,size,token_hash,plan) VALUES($1,$2,$3,$4,4,$5,$6)").bind(id.uuid()).bind(project).bind(s.uuid()).bind(allocation).bind([0u8;32].as_slice()).bind(serde_json::json!(p)).execute(&pool).await.unwrap();
    let before: Value = sqlx::query_scalar("SELECT to_jsonb(f) FROM file_uploads f")
        .fetch_one(&pool)
        .await
        .unwrap();
    let operation = snapshot(&f, id).await;
    sandbox_store::MIGRATOR.run(&pool).await.unwrap();
    sandbox_store::MIGRATOR.run(&pool).await.unwrap();
    let mut after: Value = sqlx::query_scalar("SELECT to_jsonb(f) FROM file_uploads f")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        after
            .as_object_mut()
            .unwrap()
            .remove("source_cleanup_revision"),
        Some(serde_json::json!(0))
    );
    for key in [
        "source_frozen_at",
        "source_cleanup_manifest",
        "source_cleanup_lease_until",
        "source_cleanup_next_at",
        "source_retired_at",
        "source_retirement",
    ] {
        assert_eq!(
            after.as_object_mut().unwrap().remove(key),
            Some(Value::Null)
        );
    }
    assert_eq!(before, after);
    assert_eq!(snapshot(&f, id).await, operation);
    let claim = f
        .store
        .claim_file_source_cleanup(30)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        f.store
            .prepare_file_source_cleanup(&claim)
            .await
            .unwrap()
            .manifest
            .plan,
        p
    );
    assert_eq!(unretired_bytes(&f).await, 4);
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn verified_retirement_reopens_project_byte_capacity_but_retains_guest_budget(pool: PgPool) {
    let _guard = TEST_LOCK.lock().await;
    let (f, _sources, mut c, s) = setup(&pool).await;
    let bytes = vec![5; 8 * 1024 * 1024];
    let (first, template) = aged(&f, s, bytes.clone()).await;
    tick(&f, &mut c).await; // ordinary deadline rejection, no guest attempt
    assert_eq!(state(&f, &first.to_string()).await["status"], "failed");
    let payload: Value = sqlx::query_scalar("SELECT payload FROM operations WHERE id=$1")
        .bind(first.uuid())
        .fetch_one(&pool)
        .await
        .unwrap();
    let project = template.owner.scope.project_id;
    // Valid retained history across four released allocations, each <=64 MiB.
    let mut remaining = 31;
    for _ in 0..4 {
        let sandbox = SandboxId::generate();
        let allocation = sandbox_protocol::AllocationId::generate();
        sqlx::query("INSERT INTO sandboxes(id,project_id,image_digest,resources,desired_state,observed_state,generation,destroyed_at) VALUES($1,$2,'sha256:history','{}','destroyed','destroyed',1,clock_timestamp())").bind(sandbox.uuid()).bind(project.uuid()).execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO allocations(id,project_id,sandbox_id,host_id,generation,supervisor_epoch,vcpu,memory_mib,disk_mib,status,released_at,release_evidence) VALUES($1,$2,$3,$4,1,1,1,128,64,'released',clock_timestamp(),'{\"simulated\":true}')").bind(allocation.uuid()).bind(project.uuid()).bind(sandbox.uuid()).bind(template.owner.scope.host_id.uuid()).execute(&pool).await.unwrap();
        for _ in 0..remaining.min(8) {
            let id = OperationId::generate();
            let mut p = template.clone();
            p.owner.scope.sandbox_id = sandbox;
            p.owner.scope.allocation_id = allocation;
            p.owner.operation_id = id;
            p.upload.operation_id = id;
            p.source_attempt = OperationId::generate();
            let digest = sandbox_protocol::RequestDigest::compute(
                "PUT",
                &format!("/v1/sandboxes/{sandbox}/files"),
                &payload,
            )
            .unwrap();
            sqlx::query("INSERT INTO operations(id,project_id,sandbox_id,kind,initiator_kind,idempotency_key,request_digest,digest_version,payload,status,file_allocation_id,deadline,completed_at) VALUES($1,$2,$3,'file_write','service',$4,$5,1,$6,'failed',$7,clock_timestamp()-interval '1 hour',clock_timestamp())").bind(id.uuid()).bind(project.uuid()).bind(sandbox.uuid()).bind(id.to_string()).bind(digest.as_bytes().as_slice()).bind(&payload).bind(allocation.uuid()).execute(&pool).await.unwrap();
            sqlx::query("INSERT INTO file_uploads(operation_id,project_id,sandbox_id,allocation_id,size,token_hash,plan) VALUES($1,$2,$3,$4,$5,$6,$7)").bind(id.uuid()).bind(project.uuid()).bind(sandbox.uuid()).bind(allocation.uuid()).bind(bytes.len() as i64).bind([0u8;32].as_slice()).bind(serde_json::json!(p)).execute(&pool).await.unwrap();
            remaining -= 1;
        }
    }
    assert_eq!(remaining, 0);
    assert_eq!(unretired_bytes(&f).await, 256 * 1024 * 1024);
    let key = OperationId::generate().to_string();
    assert_eq!(
        put(
            f.app.clone(),
            f.token.clone(),
            s,
            key.clone(),
            bytes.clone()
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    let claim = f
        .store
        .claim_file_source_cleanup(30)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(claim.operation_id, first);
    // Merely freezing or claiming does not refund source capacity.
    assert_eq!(
        put(
            f.app.clone(),
            f.token.clone(),
            s,
            key.clone(),
            bytes.clone()
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    f.store
        .complete_file_source_cleanup(&claim, &receipt(&template, None))
        .await
        .unwrap();
    assert_eq!(unretired_bytes(&f).await, 248 * 1024 * 1024);
    assert_eq!(
        put(f.app.clone(), f.token.clone(), s, key, bytes).await.0,
        StatusCode::ACCEPTED
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM file_uploads")
            .fetch_one(&pool)
            .await
            .unwrap(),
        33
    );
    let declared: i64 =
        sqlx::query_scalar("SELECT sum(size)::bigint FROM file_uploads WHERE allocation_id=$1")
            .bind(template.owner.scope.allocation_id.uuid())
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(declared, 16 * 1024 * 1024);
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn cleanup_rechecks_claim_after_file_and_allocation_lock_waits(pool: PgPool) {
    let _guard = TEST_LOCK.lock().await;
    let (f, _sources, _c, s) = setup(&pool).await;
    let (_, plan) = aged(&f, s, b"data".to_vec()).await;
    for table in ["file_uploads", "allocations"] {
        let claim = f
            .store
            .claim_file_source_cleanup(30)
            .await
            .unwrap()
            .unwrap();
        sqlx::query("UPDATE file_uploads SET source_cleanup_lease_until=clock_timestamp()+interval '300 milliseconds'").execute(&pool).await.unwrap();
        let mut held = pool.begin().await.unwrap();
        sqlx::query(&format!("SELECT * FROM {table} FOR UPDATE"))
            .execute(&mut *held)
            .await
            .unwrap();
        let store = f.store.clone();
        let r = receipt(&plan, None);
        let job = tokio::spawn(async move { store.complete_file_source_cleanup(&claim, &r).await });
        tokio::time::sleep(Duration::from_millis(500)).await;
        held.commit().await.unwrap();
        assert!(matches!(job.await.unwrap(), Err(DispatchError::LostClaim)));
        assert_eq!(unretired_bytes(&f).await, 4);
    }
    for sql in [
        "UPDATE file_uploads SET source_retired_at=clock_timestamp()",
        "UPDATE file_uploads SET source_cleanup_manifest=NULL",
        "UPDATE file_uploads SET source_retirement='{}'",
    ] {
        let error = sqlx::query(sql).execute(&pool).await.unwrap_err();
        assert_eq!(
            error.as_database_error().unwrap().code().as_deref(),
            Some("23514")
        );
    }
}

async fn retired_unstarted(f: &Fixture, s: SandboxId, bytes: Vec<u8>) -> (OperationId, SourcePlan) {
    let (id, p) = aged(f, s, bytes).await;
    let op = f
        .store
        .claim_next(OperationKind::FileWrite, 30)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(op.operation_id, id);
    assert!(matches!(
        f.store.prepare_upload(&op, f.config.host, 1).await.unwrap(),
        UploadAction::Rejected
    ));
    let cleanup = f
        .store
        .claim_file_source_cleanup(30)
        .await
        .unwrap()
        .unwrap();
    let work = f.store.prepare_file_source_cleanup(&cleanup).await.unwrap();
    f.store
        .complete_file_source_cleanup(&cleanup, &receipt(&p, work.manifest.selected.as_ref()))
        .await
        .unwrap();
    (id, p)
}
#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn history_file_slots_and_bytes_require_both_source_retirement_and_guest_ack(pool: PgPool) {
    use sandbox_protocol::{
        guest_model::Context,
        history::Domain,
        supervisor::{HistoryBindingObservation, HistoryObservation},
    };
    use sandbox_store::history::Preparation;
    let _guard = TEST_LOCK.lock().await;
    for (count, size) in [(16, 1), (8, 8 * 1024 * 1024)] {
        let (f, _sources, mut c, s) = setup(&pool).await;
        let mut last = None;
        for _ in 0..count {
            last = Some(retired_unstarted(&f, s, vec![7; size]).await);
        }
        let (id, p) = last.unwrap();
        let bytes:i64=sqlx::query_scalar("SELECT COALESCE(sum(size) FILTER(WHERE source_retired_at IS NULL),0)::bigint FROM file_uploads WHERE allocation_id=$1").bind(p.owner.scope.allocation_id.uuid()).fetch_one(&pool).await.unwrap();
        assert_eq!(bytes, 0);
        let (status, _) = put(
            f.app.clone(),
            f.token.clone(),
            s,
            OperationId::generate().to_string(),
            vec![7],
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        let Preparation::Binding { claim, request } = f
            .store
            .claim_history(f.config.host, 1, Domain::Files, 30, true)
            .await
            .unwrap()
            .unwrap()
        else {
            panic!("binding")
        };
        let context = Context {
            allocation_id: p.owner.scope.allocation_id,
            generation: 1,
            boot_id: "synthetic-retirement".into(),
        };
        let request = f
            .store
            .bind_history(
                &claim,
                &HistoryBindingObservation {
                    request: Some(request),
                    context: Some((&context).into()),
                    simulated: true,
                    observed_unix_ms: time::OffsetDateTime::now_utc().unix_timestamp_nanos() as i64
                        / 1_000_000,
                },
                true,
            )
            .await
            .unwrap();
        let (status, _) = put(
            f.app.clone(),
            f.token.clone(),
            s,
            OperationId::generate().to_string(),
            vec![7],
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        f.store
            .complete_history(
                &claim,
                &HistoryObservation {
                    completed: request.barrier.clone(),
                    request: Some(request),
                    simulated: true,
                    observed_unix_ms: (time::OffsetDateTime::now_utc().unix_timestamp_nanos()
                        / 1_000_000) as i64,
                },
                true,
            )
            .await
            .unwrap();
        let (_, body) = admit(&f, s, vec![7]).await;
        assert_ne!(body["operation_id"], id.to_string());
        assert_eq!(
            settle(&f, &mut c, body["operation_id"].as_str().unwrap()).await["status"],
            "succeeded"
        );
        assert_eq!(state(&f, &id.to_string()).await["status"], "failed");
        let retained: i64 =
            sqlx::query_scalar("SELECT count(*) FROM file_uploads WHERE allocation_id=$1")
                .bind(p.owner.scope.allocation_id.uuid())
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(retained, count + 1);
    }
}
#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn history_worker_refuses_fake_durable_evidence_and_defers_reserved_prefix(pool: PgPool) {
    let _guard = TEST_LOCK.lock().await;
    let (f, _sources, c, s) = setup(&pool).await;
    let (_, p) = retired_unstarted(&f, s, vec![1]).await;
    let mut worker = c.history_retirer();
    assert_eq!(
        worker.tick().await.unwrap(),
        sandbox_controller::history::HistoryTick::Idle
    );
    assert!(matches!(
        worker.tick().await,
        Err(sandbox_controller::history::HistoryError::Rpc)
    ));
    let (completed,leased,deferred):(bool,bool,bool)=sqlx::query_as("SELECT completed_through IS NOT NULL,lease_expires_at IS NOT NULL,next_retry_at>clock_timestamp() FROM allocation_history WHERE allocation_id=$1 AND domain='files'").bind(p.owner.scope.allocation_id.uuid()).fetch_one(&pool).await.unwrap();
    assert_eq!((completed, leased, deferred), (false, false, true));
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn history_committed_file_requires_source_proof_and_original_boot(pool: PgPool) {
    use sandbox_protocol::{history::Domain, supervisor::HistoryObservation};
    use sandbox_store::history::{Error as HistoryError, Preparation};
    let _guard = TEST_LOCK.lock().await;
    let (f, _sources, mut c, s) = setup(&pool).await;
    let bytes = b"committed".to_vec();
    let (key, a) = admit(&f, s, bytes.clone()).await;
    let id: OperationId = a["operation_id"].as_str().unwrap().parse().unwrap();
    assert_eq!(
        settle(&f, &mut c, &id.to_string()).await["status"],
        "succeeded"
    );
    assert!(
        f.store
            .claim_history(f.config.host, 1, Domain::Files, 30, true)
            .await
            .unwrap()
            .is_none()
    );
    // Synthetic clock advance: retain the exact plan/reference/intent binding.
    let raw: Value =
        sqlx::query_scalar("SELECT source_ref FROM file_uploads WHERE operation_id=$1")
            .bind(id.uuid())
            .fetch_one(&pool)
            .await
            .unwrap();
    let mut reference: SourceRef = serde_json::from_value(raw).unwrap();
    for t in [
        &mut reference.plan.created_unix_ms,
        &mut reference.plan.write_expires_unix_ms,
        &mut reference.plan.expires_unix_ms,
        &mut reference.plan.delete_after_unix_ms,
    ] {
        *t -= 7200000;
    }
    reference.validate().unwrap();
    sqlx::query("UPDATE file_uploads SET plan=$2,source_ref=$3 WHERE operation_id=$1")
        .bind(id.uuid())
        .bind(serde_json::json!(reference.plan))
        .bind(serde_json::json!(reference))
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE operations SET deadline=clock_timestamp()-interval '2 hours',attempt_receipts=$2 WHERE id=$1").bind(id.uuid()).bind(serde_json::json!([{"phase":"file_begin_intent","plan_sha256":reference.plan.metadata_digest().unwrap()}])).execute(&pool).await.unwrap();
    let cleanup = f
        .store
        .claim_file_source_cleanup(30)
        .await
        .unwrap()
        .unwrap();
    f.store.prepare_file_source_cleanup(&cleanup).await.unwrap();
    assert!(
        f.store
            .claim_history(f.config.host, 1, Domain::Files, 30, true)
            .await
            .unwrap()
            .is_none()
    );
    f.store
        .complete_file_source_cleanup(&cleanup, &receipt(&reference.plan, Some(&reference)))
        .await
        .unwrap();
    let Preparation::Retire { claim, request } = f
        .store
        .claim_history(f.config.host, 1, Domain::Files, 30, true)
        .await
        .unwrap()
        .unwrap()
    else {
        panic!("expected boot from committed receipt")
    };
    let saved: Value = sqlx::query_scalar("SELECT record FROM file_uploads WHERE operation_id=$1")
        .bind(id.uuid())
        .fetch_one(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE file_uploads SET record=jsonb_set(record,'{context,boot_id}','\"another-boot\"') WHERE operation_id=$1").bind(id.uuid()).execute(&pool).await.unwrap();
    let observation = HistoryObservation {
        completed: request.barrier.clone(),
        request: Some(request),
        simulated: true,
        observed_unix_ms: (time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000)
            as i64,
    };
    assert!(matches!(
        f.store.complete_history(&claim, &observation, true).await,
        Err(HistoryError::Evidence)
    ));
    sqlx::query("UPDATE file_uploads SET record=$2 WHERE operation_id=$1")
        .bind(id.uuid())
        .bind(saved)
        .execute(&pool)
        .await
        .unwrap();
    f.store
        .complete_history(&claim, &observation, true)
        .await
        .unwrap();
    let (status, retry) = put(f.app.clone(), f.token.clone(), s, key, bytes).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(retry["operation_id"], id.to_string());
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM completed_allocation_history WHERE domain='files'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(count, 1);
}

// Synthetic source and destruction proofs isolate public accounting from storage I/O.
async fn released_quota_case(pool: PgPool, global: bool) {
    use sandbox_protocol::{
        AllocationId, ProjectId,
        history::Domain,
        supervisor::{AllocationState, ReleasedHistoryObservation},
    };
    let _guard = TEST_LOCK.lock().await;
    let (f, _sources, c, s) = setup(&pool).await;
    let (first, template) = retired_unstarted(&f, s, vec![1]).await;
    let payload: Value = sqlx::query_scalar("SELECT payload FROM operations WHERE id=$1")
        .bind(first.uuid())
        .fetch_one(&pool)
        .await
        .unwrap();
    let mut remaining = if global { 1023 } else { 127 };
    let mut tx = pool.begin().await.unwrap();
    let mut project = template.owner.scope.project_id;
    let mut allocations: usize = 0;
    while remaining > 0 {
        if global && allocations.is_multiple_of(8) {
            project = ProjectId::generate();
            sqlx::query("INSERT INTO projects(id,name,status,limits,api_tokens) VALUES($1,'retired quota','active','{}','[]')").bind(project.uuid()).execute(&mut *tx).await.unwrap();
        }
        let sandbox = SandboxId::generate();
        let allocation = AllocationId::generate();
        sqlx::query("INSERT INTO sandboxes(id,project_id,image_digest,resources,desired_state,observed_state,generation,destroyed_at) VALUES($1,$2,'sha256:history','{}','destroyed','destroyed',1,clock_timestamp())").bind(sandbox.uuid()).bind(project.uuid()).execute(&mut *tx).await.unwrap();
        sqlx::query("INSERT INTO allocations(id,project_id,sandbox_id,host_id,generation,supervisor_epoch,vcpu,memory_mib,disk_mib,status,released_at,release_evidence) VALUES($1,$2,$3,$4,1,1,1,128,64,'released',clock_timestamp(),'{\"simulated\":true}')").bind(allocation.uuid()).bind(project.uuid()).bind(sandbox.uuid()).bind(f.config.host.uuid()).execute(&mut *tx).await.unwrap();
        for _ in 0..remaining.min(16) {
            let id = OperationId::generate();
            let mut p = template.clone();
            p.owner.scope.project_id = project;
            p.owner.scope.sandbox_id = sandbox;
            p.owner.scope.allocation_id = allocation;
            p.owner.operation_id = id;
            p.upload.operation_id = id;
            p.source_attempt = OperationId::generate();
            let digest = sandbox_protocol::RequestDigest::compute(
                "PUT",
                &format!("/v1/sandboxes/{sandbox}/files"),
                &payload,
            )
            .unwrap();
            sqlx::query("INSERT INTO operations(id,project_id,sandbox_id,kind,initiator_kind,idempotency_key,request_digest,digest_version,payload,status,phase,file_allocation_id,deadline,completed_at) VALUES($1,$2,$3,'file_write','service',$4,$5,1,$6,'failed','file_not_started',$7,clock_timestamp()-interval '1 hour',clock_timestamp())")
                .bind(id.uuid()).bind(project.uuid()).bind(sandbox.uuid()).bind(id.to_string()).bind(digest.as_bytes().as_slice()).bind(&payload).bind(allocation.uuid()).execute(&mut *tx).await.unwrap();
            let manifest = sandbox_store::uploads::cleanup::SourceCleanupManifest {
                version: 1,
                plan: p.clone(),
                selected: None,
            };
            sqlx::query("INSERT INTO file_uploads(operation_id,project_id,sandbox_id,allocation_id,size,token_hash,plan,source_frozen_at,source_cleanup_revision,source_cleanup_manifest,source_retired_at,source_retirement) VALUES($1,$2,$3,$4,1,$5,$6,clock_timestamp(),1,$7,clock_timestamp(),$8)")
                .bind(id.uuid()).bind(project.uuid()).bind(sandbox.uuid()).bind(allocation.uuid()).bind([0u8;32].as_slice()).bind(serde_json::json!(p)).bind(serde_json::json!(manifest)).bind(serde_json::json!(receipt(&p,None))).execute(&mut *tx).await.unwrap();
            remaining -= 1;
        }
        allocations += 1;
    }
    tx.commit().await.unwrap();
    let key = OperationId::generate().to_string();
    assert_eq!(
        put(f.app.clone(), f.token.clone(), s, key.clone(), vec![1])
            .await
            .0,
        StatusCode::CONFLICT
    );
    // A fake supervisor cannot manufacture a durable destruction acknowledgement.
    let mut worker = c.history_retirer();
    for _ in 0..3 {
        let _ = worker.tick().await;
    }
    assert!(matches!(
        worker.tick().await,
        Err(sandbox_controller::history::HistoryError::Rpc)
    ));
    sqlx::query("UPDATE released_allocation_history SET next_retry_at=NULL")
        .execute(&pool)
        .await
        .unwrap();
    let p = f
        .store
        .claim_released_history(f.config.host, 1, Domain::Files, 30, true)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        put(f.app.clone(), f.token.clone(), s, key.clone(), vec![1])
            .await
            .0,
        StatusCode::CONFLICT
    );
    let mut observed = ReleasedHistoryObservation {
        completed_through: p.request.through.clone(),
        request: Some(p.request.clone()),
        release_state: AllocationState::Released as i32,
        simulated: true,
        observed_unix_ms: (time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000)
            as i64,
    };
    observed.release_state = AllocationState::Absent as i32;
    assert!(
        f.store
            .complete_released_history(&p.claim, &observed, true)
            .await
            .is_err()
    );
    assert_eq!(
        put(f.app.clone(), f.token.clone(), s, key.clone(), vec![1])
            .await
            .0,
        StatusCode::CONFLICT
    );
    observed.release_state = AllocationState::Released as i32;
    let (covered, saved): (sqlx::types::Uuid, Value) = sqlx::query_as("SELECT operation_id,source_retirement FROM file_uploads WHERE allocation_id=$1 ORDER BY operation_id LIMIT 1")
        .bind(p.claim.claim.allocation_id.uuid()).fetch_one(&pool).await.unwrap();
    sqlx::query("UPDATE file_uploads SET source_retirement=jsonb_set(source_retirement,'{plan_sha256}',to_jsonb($2::text)) WHERE operation_id=$1")
        .bind(covered).bind("0".repeat(64)).execute(&pool).await.unwrap();
    assert!(
        f.store
            .complete_released_history(&p.claim, &observed, true)
            .await
            .is_err()
    );
    assert_eq!(
        put(f.app.clone(), f.token.clone(), s, key.clone(), vec![1])
            .await
            .0,
        StatusCode::CONFLICT
    );
    sqlx::query("UPDATE file_uploads SET source_retirement=$2 WHERE operation_id=$1")
        .bind(covered)
        .bind(saved)
        .execute(&pool)
        .await
        .unwrap();
    f.store
        .complete_released_history(&p.claim, &observed, true)
        .await
        .unwrap();
    assert_eq!(
        put(f.app.clone(), f.token.clone(), s, key, vec![1]).await.0,
        StatusCode::ACCEPTED
    );
    let retained: i64 = sqlx::query_scalar("SELECT count(*) FROM file_uploads")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(retained, if global { 1025 } else { 129 });
}
#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn released_history_restores_project_slots_only_after_destruction_ack(pool: PgPool) {
    released_quota_case(pool, false).await;
}
#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn released_history_restores_global_slots_only_after_destruction_ack(pool: PgPool) {
    released_quota_case(pool, true).await;
}
