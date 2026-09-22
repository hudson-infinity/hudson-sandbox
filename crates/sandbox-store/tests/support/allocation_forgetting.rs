//! Synthetic observations test SQL handoffs, not physical cleanup.
use super::*;
use sandbox_protocol::{
    allocation_retirement::ForgetRequest,
    supervisor::{AllocationForgetObservation, AllocationForgetRequest, AllocationForgetState},
};
use serde_json::Value;
async fn metadata(f: &Fixture) -> sandbox_protocol::allocation_retirement::Request {
    let r = prepare(f).await.unwrap().unwrap();
    f.store
        .complete_allocation_retirement(&r, &super::completion::observation(&r))
        .await
        .unwrap();
    r
}
async fn claim(f: &Fixture, epoch: i64, seconds: u32) -> ForgetRequest {
    f.store
        .prepare_allocation_forgetting(f.allocation, f.host, epoch, seconds)
        .await
        .unwrap()
        .unwrap()
}
fn observation(r: &ForgetRequest, state: AllocationForgetState) -> AllocationForgetObservation {
    AllocationForgetObservation {
        request: Some(AllocationForgetRequest {
            request_json: r.encode().unwrap(),
        }),
        state: state as i32,
        observed_unix_ms: now_ms(),
    }
}
#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn forgetting_requires_metadata_completion_and_keeps_original_proofs_and_results(
    pool: PgPool,
) {
    let f = Fixture::new(&pool).await;
    released(&f).await;
    assert!(
        f.store
            .prepare_allocation_forgetting(f.allocation, f.host, 1, 30)
            .await
            .is_err()
    );
    let r = metadata(&f).await;
    let before: Value =
        sqlx::query_scalar("SELECT jsonb_agg(to_jsonb(o) ORDER BY id) FROM operations o")
            .fetch_one(&pool)
            .await
            .unwrap();
    let retained = f
        .store
        .allocation_retirement_completion(&r.intent)
        .await
        .unwrap();
    let request = claim(&f, 1, 30).await;
    assert_eq!(request.metadata_request, r);
    assert!(
        f.store
            .prepare_allocation_forgetting(f.allocation, f.host, 1, 30)
            .await
            .unwrap()
            .is_none()
    );
    let o = observation(&request, AllocationForgetState::Retired);
    let done = f
        .store
        .complete_allocation_forgetting(&request, &o)
        .await
        .unwrap();
    sqlx::query("UPDATE hosts SET supervisor_epoch=2")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        f.store
            .allocation_forgetting_completion(&r.intent)
            .await
            .unwrap(),
        Some(done.clone())
    );
    assert_eq!(
        f.store
            .complete_allocation_forgetting(&request, &o)
            .await
            .unwrap(),
        done
    );
    assert!(
        f.store
            .prepare_allocation_forgetting(f.allocation, f.host, 2, 30)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        f.store
            .allocation_retirement_completion(&r.intent)
            .await
            .unwrap(),
        retained
    );
    let after: Value =
        sqlx::query_scalar("SELECT jsonb_agg(to_jsonb(o) ORDER BY id) FROM operations o")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(before, after);
}
#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn changed_fresh_and_historical_claims_or_unknown_responses_cannot_forget(pool: PgPool) {
    let f = Fixture::new(&pool).await;
    released(&f).await;
    metadata(&f).await;
    let r = claim(&f, 1, 30).await;
    for mode in 0..8 {
        let mut wrong = r.clone();
        match mode {
            0 => wrong.claim.revision += 1,
            1 => wrong.claim.expires_unix_ms += 1,
            2 => wrong.claim.reporting_epoch += 1,
            3 => wrong.metadata_request.revision += 1,
            4 => wrong.metadata_request.expires_unix_ms += 1,
            5 => {
                wrong.claim.intent.release_evidence_sha256 = "cd".repeat(32);
                wrong.metadata_request.intent = wrong.claim.intent.clone();
            }
            6 => {
                wrong.claim.intent.permit.project = ProjectId::generate();
                wrong.metadata_request.intent = wrong.claim.intent.clone();
            }
            _ => {
                wrong.claim.intent.commands = DomainClosure::Retired {
                    through: OperationId::generate(),
                };
                wrong.metadata_request.intent = wrong.claim.intent.clone();
            }
        }
        assert!(
            f.store
                .complete_allocation_forgetting(
                    &wrong,
                    &observation(&wrong, AllocationForgetState::Forgotten)
                )
                .await
                .is_err(),
            "mode {mode}"
        );
    }
    for mode in 0..5 {
        let mut o = observation(&r, AllocationForgetState::Forgotten);
        match mode {
            0 => o.state = 0,
            1 => o.request = None,
            2 => o.observed_unix_ms = 1,
            3 => o.observed_unix_ms = now_ms() + 20000,
            _ => o.request.as_mut().unwrap().request_json.push(b' '),
        };
        assert!(
            f.store
                .complete_allocation_forgetting(&r, &o)
                .await
                .is_err()
        );
    }
    assert!(
        f.store
            .allocation_forgetting_completion(&r.claim.intent)
            .await
            .unwrap()
            .is_none()
    );
    f.store
        .complete_allocation_forgetting(&r, &observation(&r, AllocationForgetState::Forgotten))
        .await
        .unwrap();
}
#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn concurrent_forgetting_completion_is_one_durable_result(pool: PgPool) {
    let f = Fixture::new(&pool).await;
    released(&f).await;
    metadata(&f).await;
    let r = claim(&f, 1, 30).await;
    let o = observation(&r, AllocationForgetState::Forgotten);
    let mut tasks = tokio::task::JoinSet::new();
    for _ in 0..8 {
        let store = f.store.clone();
        let r = r.clone();
        let o = o.clone();
        tasks.spawn(async move { store.complete_allocation_forgetting(&r, &o).await.unwrap() });
    }
    let first = tasks.join_next().await.unwrap().unwrap();
    while let Some(next) = tasks.join_next().await {
        assert_eq!(first, next.unwrap());
    }
}
#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn forgetting_expiry_during_lock_wait_is_checked_at_the_write(pool: PgPool) {
    let f = Fixture::new(&pool).await;
    released(&f).await;
    metadata(&f).await;
    let r = claim(&f, 1, 2).await;
    let mut lock = pool.begin().await.unwrap();
    sqlx::query("SELECT id FROM allocations WHERE id=$1 FOR UPDATE")
        .bind(f.allocation.uuid())
        .execute(&mut *lock)
        .await
        .unwrap();
    let store = f.store.clone();
    let old = r.clone();
    let task = tokio::spawn(async move {
        store
            .complete_allocation_forgetting(
                &old,
                &observation(&old, AllocationForgetState::Retired),
            )
            .await
    });
    super::completion::blocked(&pool, "SELECT * FROM allocations WHERE id=% FOR UPDATE").await;
    sqlx::query("SELECT pg_sleep(GREATEST(extract(epoch FROM (forget_lease_expires_at-clock_timestamp())),0)+0.02) FROM allocation_retirements").execute(&pool).await.unwrap();
    lock.commit().await.unwrap();
    assert!(matches!(task.await.unwrap(), Err(Error::LostClaim)));
    assert!(
        f.store
            .allocation_forgetting_completion(&r.claim.intent)
            .await
            .unwrap()
            .is_none()
    );
    let new = claim(&f, 1, 30).await;
    assert_eq!(new.claim.revision, r.claim.revision + 1);
    assert_eq!(new.metadata_request, r.metadata_request);
    assert!(
        f.store
            .complete_allocation_forgetting(&r, &observation(&r, AllocationForgetState::Retired))
            .await
            .is_err()
    );
    f.store
        .complete_allocation_forgetting(&new, &observation(&new, AllocationForgetState::Retired))
        .await
        .unwrap();
}
#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn forgetting_epoch_race_and_consumer_changes_reject_acknowledgement(pool: PgPool) {
    let f = Fixture::new(&pool).await;
    let command = f.admit().await;
    super::super::history::finish(&f, command).await;
    released(&f).await;
    history_done(&f).await;
    metadata(&f).await;
    let r = claim(&f, 1, 30).await;
    let mut epoch = pool.begin().await.unwrap();
    sqlx::query("UPDATE hosts SET supervisor_epoch=2 WHERE id=$1")
        .bind(f.host.uuid())
        .execute(&mut *epoch)
        .await
        .unwrap();
    let store = f.store.clone();
    let old = r.clone();
    let task = tokio::spawn(async move {
        store
            .complete_allocation_forgetting(
                &old,
                &observation(&old, AllocationForgetState::Retired),
            )
            .await
    });
    super::completion::blocked(&pool, "SELECT id FROM hosts WHERE id=% FOR SHARE").await;
    epoch.commit().await.unwrap();
    assert!(matches!(task.await.unwrap(), Err(Error::LostClaim)));
    sqlx::query("UPDATE allocation_retirements SET forget_lease_expires_at=clock_timestamp()-interval '1 second'").execute(&pool).await.unwrap();
    let new = claim(&f, 2, 30).await;
    let original: Value = sqlx::query_scalar("SELECT release_evidence FROM allocations")
        .fetch_one(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE allocations SET release_evidence=jsonb_set(release_evidence,'{observed_unix_ms}','1')").execute(&pool).await.unwrap();
    assert!(
        f.store
            .complete_allocation_forgetting(
                &new,
                &observation(&new, AllocationForgetState::Retired)
            )
            .await
            .is_err()
    );
    sqlx::query("UPDATE allocations SET release_evidence=$1")
        .bind(original)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE operations SET output_status='pending' WHERE id=$1")
        .bind(command.uuid())
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        f.store
            .complete_allocation_forgetting(
                &new,
                &observation(&new, AllocationForgetState::Retired)
            )
            .await
            .is_err()
    );
    sqlx::query("UPDATE operations SET output_status='none' WHERE id=$1")
        .bind(command.uuid())
        .execute(&pool)
        .await
        .unwrap();
    f.store
        .complete_allocation_forgetting(&new, &observation(&new, AllocationForgetState::Retired))
        .await
        .unwrap();
}
#[sqlx::test(migrations = false)]
async fn forgetting_upgrade_preserves_completed_metadata_without_inventing_forgetting(
    pool: PgPool,
) {
    let previous = sqlx::migrate::Migrator {
        migrations: std::borrow::Cow::Owned(
            sandbox_store::MIGRATOR.iter().take(20).cloned().collect(),
        ),
        ..sqlx::migrate::Migrator::DEFAULT
    };
    previous.run(&pool).await.unwrap();
    let mut f = Fixture::new(&pool).await;
    released(&f).await;
    let r = metadata(&f).await;
    let before: Value = sqlx::query_scalar("SELECT to_jsonb(r) FROM allocation_retirements r")
        .fetch_one(&pool)
        .await
        .unwrap();
    sandbox_store::MIGRATOR.run(&pool).await.unwrap();
    sandbox_store::MIGRATOR.run(&pool).await.unwrap();
    let pool = reconnect_after_upgrade(&pool).await;
    f.store = Store::from_pool(pool.clone());
    let after: Value = sqlx::query_scalar("SELECT to_jsonb(r) FROM allocation_retirements r")
        .fetch_one(&pool)
        .await
        .unwrap();
    for (key, value) in before.as_object().unwrap() {
        assert_eq!(&after[key], value, "field {key}");
    }
    assert_eq!(after["forget_revision"], 0);
    assert!(after["forget_epoch"].is_null());
    assert!(after["forget_completion"].is_null());
    assert!(
        f.store
            .allocation_forgetting_completion(&r.intent)
            .await
            .unwrap()
            .is_none()
    );
    let request = claim(&f, 1, 30).await;
    let o = observation(&request, AllocationForgetState::Forgotten);
    f.store
        .complete_allocation_forgetting(&request, &o)
        .await
        .unwrap();
    let original: Value =
        sqlx::query_scalar("SELECT forget_completion FROM allocation_retirements")
            .fetch_one(&pool)
            .await
            .unwrap();
    for field in ["revision", "reporting_epoch", "expires_unix_ms"] {
        let mut bad = original.clone();
        bad["request"]["claim"][field] = 0.into();
        assert!(
            sqlx::query("UPDATE allocation_retirements SET forget_completion=$1")
                .bind(bad)
                .execute(&pool)
                .await
                .is_err()
        );
    }
    sqlx::query("UPDATE allocation_retirements SET forget_completion=jsonb_set(forget_completion,'{observed_unix_ms}','1')").execute(&pool).await.unwrap();
    assert!(
        f.store
            .allocation_forgetting_completion(&r.intent)
            .await
            .is_err()
    );
}

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn historical_completion_cannot_have_been_accepted_after_its_claim_expired(pool: PgPool) {
    let f = Fixture::new(&pool).await;
    released(&f).await;
    metadata(&f).await;
    let r = claim(&f, 1, 2).await;
    f.store
        .complete_allocation_forgetting(&r, &observation(&r, AllocationForgetState::Retired))
        .await
        .unwrap();
    sqlx::query("UPDATE allocation_retirements SET forgotten_at=forget_claim_expires_at")
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        f.store
            .allocation_forgetting_completion(&r.claim.intent)
            .await
            .is_err()
    );
    assert!(
        f.store
            .prepare_allocation_forgetting(f.allocation, f.host, 1, 30)
            .await
            .is_err()
    );
}
