//! Synthetic terminal history exercises admission, not actual guest execution.
use super::*;
use sandbox_protocol::{command::MAX_COMMANDS, guest_model::MAX_RESERVED_OUTPUT};
use sandbox_store::compaction::Compaction;

async fn finish(f: &Fixture, id: OperationId) {
    sqlx::query(
        r#"UPDATE operations SET status='failed',phase='rejected_before_dispatch',
        completed_at=clock_timestamp(),lease_expires_at=NULL,next_retry_at=NULL,
        error='{"code":"execution_authority_expired","dispatch_intent_absent":true}' WHERE id=$1"#,
    )
    .bind(id.uuid())
    .execute(f.store.pool())
    .await
    .unwrap();
}
async fn fill(f: &mut Fixture, count: usize, output: u64) -> Vec<OperationId> {
    f.request.command.output_limit = output;
    let mut ids = Vec::new();
    for _ in 0..count {
        f.request.idempotency_key = key();
        let id = f.admit().await;
        finish(f, id).await;
        ids.push(id);
    }
    ids
}
async fn count(f: &Fixture) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM operations WHERE execution_allocation_id=$1")
        .bind(f.allocation.uuid())
        .fetch_one(f.store.pool())
        .await
        .unwrap()
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn retained_slot_boundary_preserves_retries_and_destroy(pool: PgPool) {
    let mut f = Fixture::new(&pool).await;
    let ids = fill(&mut f, MAX_COMMANDS, 1).await;
    assert_eq!(
        f.store.admit_execute(&f.request).await.unwrap(),
        ExecuteAdmission::Accepted {
            operation_id: *ids.last().unwrap(),
            status: "failed".into()
        }
    );
    let mut changed = f.request.clone();
    changed.command.output_limit = 2;
    assert_eq!(
        f.store.admit_execute(&changed).await.unwrap(),
        ExecuteAdmission::DigestConflict
    );
    f.request.idempotency_key = key();
    assert_eq!(
        f.store.admit_execute(&f.request).await.unwrap(),
        ExecuteAdmission::CapacityExhausted
    );
    assert_eq!(count(&f).await, MAX_COMMANDS as i64);
    let _ = f.destroy().await;
    assert_eq!(count(&f).await, MAX_COMMANDS as i64);
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn byte_budget_is_exact_and_rejection_does_not_consume_a_key(pool: PgPool) {
    let mut f = Fixture::new(&pool).await;
    fill(&mut f, 6, 10 * 1024 * 1024).await;
    f.request.idempotency_key = key();
    f.request.command.output_limit = 4 * 1024 * 1024 + 1;
    assert_eq!(
        f.store.admit_execute(&f.request).await.unwrap(),
        ExecuteAdmission::CapacityExhausted
    );
    assert_eq!(count(&f).await, 6);
    // This same key was not consumed: the smaller request fits exactly.
    f.request.command.output_limit -= 1;
    let id = f.admit().await;
    finish(&f, id).await;
    f.request.idempotency_key = key();
    f.request.command.output_limit = 1;
    assert_eq!(
        f.store.admit_execute(&f.request).await.unwrap(),
        ExecuteAdmission::CapacityExhausted
    );
    assert_eq!(count(&f).await, 7);
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn concurrent_last_slot_is_reserved_once_and_survives_unknown(pool: PgPool) {
    let mut f = Fixture::new(&pool).await;
    fill(&mut f, MAX_COMMANDS - 1, 1).await;
    let mut tasks = tokio::task::JoinSet::new();
    for _ in 0..8 {
        let store = f.store.clone();
        let mut r = f.request.clone();
        r.idempotency_key = key();
        tasks.spawn(async move { store.admit_execute(&r).await.unwrap() });
    }
    let mut accepted = Vec::new();
    let mut busy = 0;
    while let Some(result) = tasks.join_next().await {
        match result.unwrap() {
            ExecuteAdmission::Accepted { operation_id, .. } => accepted.push(operation_id),
            ExecuteAdmission::Busy(_) => busy += 1,
            other => panic!("{other:?}"),
        }
    }
    assert_eq!((accepted.len(), busy), (1, 7));
    let claim = f.claim().await;
    assert!(matches!(
        f.store.prepare_execute(&claim, f.host, 1).await.unwrap(),
        ExecuteAction::Dispatch { .. }
    ));
    f.store.record_execute_unknown(&claim).await.unwrap();
    f.request.idempotency_key = key();
    assert_eq!(
        f.store.admit_execute(&f.request).await.unwrap(),
        ExecuteAdmission::Busy(accepted[0])
    );
    sqlx::query(
        "UPDATE operations SET next_retry_at=clock_timestamp()-interval '1 second' WHERE id=$1",
    )
    .bind(accepted[0].uuid())
    .execute(&pool)
    .await
    .unwrap();
    let replacement = f.claim().await;
    assert!(matches!(
        f.store
            .prepare_execute(&replacement, f.host, 1)
            .await
            .unwrap(),
        ExecuteAction::Inspect { .. }
    ));
    assert_eq!(count(&f).await, MAX_COMMANDS as i64);
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn response_compaction_preserves_reservations_and_old_retry_identity(pool: PgPool) {
    let mut f = Fixture::new(&pool).await;
    let ids = fill(&mut f, 7, MAX_RESERVED_OUTPUT / 8).await;
    let old = f.request.clone();
    sqlx::query("UPDATE operations SET response_expires_at=clock_timestamp()-interval '1 second' WHERE execution_allocation_id=$1")
        .bind(f.allocation.uuid()).execute(&pool).await.unwrap();
    for id in &ids {
        assert_eq!(
            f.store.compact_expired_response().await.unwrap(),
            Compaction::Completed(*id)
        );
    }
    let empty: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM operations WHERE payload='{}' AND payload_compacted_at IS NOT NULL",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(empty, 7);
    sqlx::query("UPDATE operations SET command_summary=jsonb_set(command_summary,'{output_limit}','0') WHERE id=$1")
        .bind(ids[0].uuid()).execute(&pool).await.unwrap();
    f.request.idempotency_key = key();
    assert!(matches!(
        f.store.admit_execute(&f.request).await,
        Err(DispatchError::InvalidData)
    ));
    sqlx::query("UPDATE operations SET command_summary=jsonb_set(command_summary,'{output_limit}',to_jsonb($2::bigint)) WHERE id=$1")
        .bind(ids[0].uuid()).bind((MAX_RESERVED_OUTPUT / 8) as i64).execute(&pool).await.unwrap();
    f.request.idempotency_key = key();
    f.request.command.output_limit += 1;
    assert_eq!(
        f.store.admit_execute(&f.request).await.unwrap(),
        ExecuteAdmission::CapacityExhausted
    );
    f.request.command.output_limit -= 1;
    let id = f.admit().await;
    finish(&f, id).await;
    assert_eq!(
        f.store.admit_execute(&old).await.unwrap(),
        ExecuteAdmission::ResponseExpired(ids[6])
    );
    f.request.idempotency_key = key();
    f.request.command.output_limit = 1;
    assert_eq!(
        f.store.admit_execute(&f.request).await.unwrap(),
        ExecuteAdmission::CapacityExhausted
    );
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn malformed_history_fails_closed_without_consuming_capacity_or_keys(pool: PgPool) {
    let mut f = Fixture::new(&pool).await;
    let id = fill(&mut f, 1, 1024).await[0];
    let original: serde_json::Value =
        sqlx::query_scalar("SELECT payload FROM operations WHERE id=$1")
            .bind(id.uuid())
            .fetch_one(&pool)
            .await
            .unwrap();
    f.request.idempotency_key = key();
    for payload in [
        json!({}),
        json!({"argv":["true"],"deadline_unix_ms":1,"output_limit":0}),
        json!({"argv":["true"],"deadline_unix_ms":1,"output_limit":u64::MAX}),
    ] {
        sqlx::query("UPDATE operations SET payload=$2 WHERE id=$1")
            .bind(id.uuid())
            .bind(payload)
            .execute(&pool)
            .await
            .unwrap();
        assert!(matches!(
            f.store.admit_execute(&f.request).await,
            Err(DispatchError::InvalidData)
        ));
        assert_eq!(count(&f).await, 1);
    }
    sqlx::query("UPDATE operations SET payload=$2 WHERE id=$1")
        .bind(id.uuid())
        .bind(original)
        .execute(&pool)
        .await
        .unwrap();
    let _ = f.admit().await;
    assert_eq!(count(&f).await, 2);
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn capacity_is_per_allocation_and_tenant_checks_still_come_first(pool: PgPool) {
    let mut full = Fixture::new(&pool).await;
    fill(&mut full, MAX_COMMANDS, 1).await;
    let other = Fixture::new(&pool).await;
    let mut foreign = other.request.clone();
    foreign.sandbox_id = full.request.sandbox_id;
    assert_eq!(
        other.store.admit_execute(&foreign).await.unwrap(),
        ExecuteAdmission::NotFound
    );
    let _ = other.admit().await;
    full.request.idempotency_key = key();
    sqlx::query("UPDATE projects SET api_tokens='[]' WHERE id=$1")
        .bind(full.request.project_id.uuid())
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        full.store.admit_execute(&full.request).await.unwrap(),
        ExecuteAdmission::Unauthorized
    );
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn legacy_pressure_never_turns_an_unknown_dispatch_into_a_rejection(pool: PgPool) {
    let mut f = Fixture::new(&pool).await;
    let ids = fill(&mut f, MAX_COMMANDS - 1, 1).await;
    f.request.idempotency_key = key();
    let id = f.admit().await;
    let claim = f.claim().await;
    assert!(matches!(
        f.store.prepare_execute(&claim, f.host, 1).await.unwrap(),
        ExecuteAction::Dispatch { .. }
    ));
    f.store.record_execute_unknown(&claim).await.unwrap();
    // Additional history from a previous writer exceeds today's admission cap.
    sqlx::query("INSERT INTO operations(id,project_id,sandbox_id,kind,initiator_kind,initiator_key_id,idempotency_key,
        request_digest,digest_version,payload,status,completed_at,execution_allocation_id)
        SELECT $1,project_id,sandbox_id,kind,initiator_kind,initiator_key_id,$2,
        request_digest,digest_version,payload,'failed',clock_timestamp(),execution_allocation_id FROM operations WHERE id=$3")
        .bind(OperationId::generate().uuid()).bind(key().as_str()).bind(ids[0].uuid()).execute(&pool).await.unwrap();
    assert_eq!(count(&f).await, MAX_COMMANDS as i64 + 1);
    sqlx::query(
        "UPDATE operations SET next_retry_at=clock_timestamp()-interval '1 second' WHERE id=$1",
    )
    .bind(id.uuid())
    .execute(&pool)
    .await
    .unwrap();
    let replacement = f.claim().await;
    assert!(matches!(
        f.store
            .prepare_execute(&replacement, f.host, 1)
            .await
            .unwrap(),
        ExecuteAction::Inspect { .. }
    ));
    let (status, attempts, complete): (String, i32, bool) = sqlx::query_as(
        "SELECT status,attempt_count,completed_at IS NOT NULL FROM operations WHERE id=$1",
    )
    .bind(id.uuid())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!((status.as_str(), attempts, complete), ("unknown", 1, false));
}

#[sqlx::test(migrations = false)]
async fn capacity_upgrade_preserves_history_and_fences_legacy_queued_dispatch(pool: PgPool) {
    let old = sqlx::migrate::Migrator {
        migrations: std::borrow::Cow::Owned(
            sandbox_store::MIGRATOR.iter().take(10).cloned().collect(),
        ),
        ..sqlx::migrate::Migrator::DEFAULT
    };
    old.run(&pool).await.unwrap();
    let mut f = Fixture::new(&pool).await;
    let ids = fill(&mut f, MAX_COMMANDS, 1).await;
    // Seed the 33rd accepted row using the old schema, bypassing new admission.
    let legacy = OperationId::generate();
    sqlx::query("INSERT INTO operations(id,project_id,sandbox_id,kind,initiator_kind,initiator_key_id,idempotency_key,
        request_digest,digest_version,payload,status,phase,deadline,execution_allocation_id)
        SELECT $1,project_id,sandbox_id,kind,initiator_kind,initiator_key_id,$2,
        request_digest,digest_version,payload,'queued','admitted',deadline,execution_allocation_id FROM operations WHERE id=$3")
        .bind(legacy.uuid()).bind(key().as_str()).bind(ids[0].uuid()).execute(&pool).await.unwrap();
    let before: Vec<serde_json::Value> =
        sqlx::query_scalar("SELECT to_jsonb(o) FROM operations o ORDER BY id")
            .fetch_all(&pool)
            .await
            .unwrap();
    sandbox_store::MIGRATOR.run(&pool).await.unwrap();
    let after: Vec<serde_json::Value> =
        sqlx::query_scalar("SELECT to_jsonb(o)-'file_allocation_id' FROM operations o ORDER BY id")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(before, after);
    let claim = f.claim().await;
    assert_eq!(claim.operation_id, legacy);
    assert!(matches!(
        f.store.prepare_execute(&claim, f.host, 1).await.unwrap(),
        ExecuteAction::Rejected
    ));
    let (status, attempts, receipts, error): (String, i32, serde_json::Value, serde_json::Value) =
        sqlx::query_as(
            "SELECT status,attempt_count,attempt_receipts,error FROM operations WHERE id=$1",
        )
        .bind(legacy.uuid())
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(status, "failed");
    assert_eq!(attempts, 0);
    assert_eq!(receipts, json!([]));
    assert_eq!(
        error,
        json!({"code":"execution_capacity_exhausted","dispatch_intent_absent":true})
    );
    f.request.idempotency_key = key();
    assert_eq!(
        f.store.admit_execute(&f.request).await.unwrap(),
        ExecuteAdmission::CapacityExhausted
    );
    let _ = f.destroy().await;
}
