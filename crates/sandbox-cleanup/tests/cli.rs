//! Actual executable startup against an isolated SQLx database. Empty work
//! must not contact the synthetic storage endpoint or leak its credentials.
#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used)]
use sqlx::PgPool;
use std::{os::unix::fs::PermissionsExt, time::Duration};

#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn once_uses_private_configuration_and_an_isolated_database(pool: PgPool) {
    let mut url = url::Url::parse(&std::env::var("DATABASE_URL").unwrap()).unwrap();
    url.set_path(pool.connect_options().get_database().unwrap());
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("storage.json");
    std::fs::write(&config, br#"{"endpoint":"https://objects.example.test","region":"us-east-1","bucket":"output-test","access_key":"test-private-access","secret_key":"test-private-secret"}"#).unwrap();
    for private in [true, false] {
        std::fs::set_permissions(
            &config,
            std::fs::Permissions::from_mode(if private { 0o600 } else { 0o644 }),
        )
        .unwrap();
        let output = tokio::time::timeout(
            Duration::from_secs(10),
            tokio::process::Command::new(env!("CARGO_BIN_EXE_sandbox-cleanup"))
                .env("DATABASE_URL", url.as_str())
                .args(["--once", "--output-config"])
                .arg(&config)
                .kill_on_drop(true)
                .output(),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(output.status.success(), private);
        let stderr = String::from_utf8(output.stderr).unwrap();
        let stdout = String::from_utf8(output.stdout).unwrap();
        if private {
            assert!(stderr.contains("Idle"));
        }
        for secret in ["test-private-access", "test-private-secret", url.as_str()] {
            assert!(!stderr.contains(secret) && !stdout.contains(secret));
        }
    }
    // PostgreSQL includes the requested database name in startup errors.
    // That provider diagnostic must not reach this operator-facing process.
    std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o600)).unwrap();
    let private_database = format!(
        "private-missing-{}",
        pool.connect_options()
            .get_database()
            .unwrap()
            .chars()
            .take(20)
            .collect::<String>()
    );
    url.set_path(&private_database);
    let output = tokio::time::timeout(
        Duration::from_secs(10),
        tokio::process::Command::new(env!("CARGO_BIN_EXE_sandbox-cleanup"))
            .env("DATABASE_URL", url.as_str())
            .args(["--once", "--output-config"])
            .arg(&config)
            .kill_on_drop(true)
            .output(),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(!stderr.contains(&private_database));
    assert!(stderr.contains("database migration failed"));

    // Exercise policy assignment through the executable, without any object
    // plan or network authority. Missing policy must preserve the old default.
    use sandbox_protocol::{Id, OperationId, ProjectId, SandboxId};
    let project = ProjectId::generate();
    let sandbox = SandboxId::generate();
    let operation = OperationId::generate();
    sqlx::query(
        "INSERT INTO projects(id,name,status,limits) VALUES($1,'cli-retention','active','{}')",
    )
    .bind(project.uuid())
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO sandboxes(id,project_id,image_digest,resources,desired_state,observed_state) VALUES($1,$2,'sha256:fixture','{}','destroyed','destroyed')")
        .bind(sandbox.uuid()).bind(project.uuid()).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO operations(id,project_id,sandbox_id,kind,initiator_kind,idempotency_key,request_digest,digest_version,payload,status,completed_at) VALUES($1,$2,$3,'create','service',$4,$5,1,'{}','failed',clock_timestamp()-interval '1 hour')")
        .bind(operation.uuid()).bind(project.uuid()).bind(sandbox.uuid()).bind(operation.to_string()).bind([1u8;32].as_slice()).execute(&pool).await.unwrap();
    url.set_path(pool.connect_options().get_database().unwrap());
    for seconds in [None, Some("0"), Some("31536001"), Some("60")] {
        let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_sandbox-cleanup"));
        command
            .env("DATABASE_URL", url.as_str())
            .args(["--once", "--output-config"])
            .arg(&config)
            .kill_on_drop(true);
        if let Some(seconds) = seconds {
            command.args(["--response-retention-seconds", seconds]);
        }
        let output = tokio::time::timeout(Duration::from_secs(10), command.output())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            output.status.success(),
            seconds.is_none() || seconds == Some("60")
        );
        let assigned: bool = sqlx::query_scalar(
            "SELECT response_expires_at IS NOT NULL FROM operations WHERE id=$1",
        )
        .bind(operation.uuid())
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(assigned, seconds == Some("60"));
        let stderr = String::from_utf8(output.stderr).unwrap();
        if seconds == Some("60") {
            assert!(stderr.contains("RetentionAssigned(1)"));
        }
    }
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM output_cleanup")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
}
