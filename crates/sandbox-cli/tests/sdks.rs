//! Python/TypeScript -> HTTPS -> real API/PostgreSQL; synthetic guest/storage adapters.
#![allow(clippy::unwrap_used, clippy::expect_used)]
#[path = "../../sandbox-api/tests/support/contract_backends.rs"]
mod backends;
#[path = "../../sandbox-client/tests/support/external.rs"]
mod external;
#[path = "../../sandbox-api/tests/support/output.rs"]
mod fixture;
#[path = "../../sandbox-client/tests/support/server.rs"]
mod server;
use fixture::Fixture;
use sandbox_protocol::Id;
use serde_json::{Value, json};
use server::{Server, private};
use sqlx::PgPool;
use std::sync::Arc;
fn credential(s: &Server, f: &Fixture) {
    private(&s.directory.path().join("credential.json"),&serde_json::to_vec(&json!({"version":1,"project_id":f.project.to_string(),"name":"fixture","token":f.token,"created_at":1,"expires_at":4102444800_i64})).unwrap());
}
async fn canonical(pool: &PgPool, f: &Fixture, digest: &str) {
    sqlx::query(
        "UPDATE sandboxes SET image_digest=$1,resources=$2,observation_simulated=true WHERE id=$3",
    )
    .bind(digest)
    .bind(json!({"vcpu":1,"memory_mib":128,"disk_mib":64}))
    .bind(f.sandbox.uuid())
    .execute(pool)
    .await
    .unwrap();
}
async fn run(s: &Server, language: &str, action: &str, args: Value) -> Value {
    external::run(
        language,
        &s.config,
        &json!({"name":action,"action":action,"args":args}),
    )
    .await
}
async fn ok(s: &Server, language: &str, action: &str, args: Value) -> Value {
    let result = run(s, language, action, args).await;
    assert!(result.get("ok").is_some(), "{language} {action}: {result}");
    result["ok"].clone()
}
#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn python_covers_all_routes_and_retries(pool: PgPool) {
    exercise(pool, "python").await;
}
#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
async fn typescript_covers_all_routes_and_retries(pool: PgPool) {
    exercise(pool, "typescript").await;
}
async fn exercise(pool: PgPool, language: &str) {
    let f = Fixture::new(&pool).await;
    f.publish().await;
    let digest = format!("sha256:{}", "a".repeat(64));
    canonical(&pool, &f, &digest).await;
    let backend = Arc::new(backends::Backend);
    let s = Server::start(sandbox_api::router_with_uploads(
        sandbox_api::AppState {
            store: f.store.clone(),
            images: sandbox_protocol::images::ImageAllowlist::new([digest.clone()]).unwrap(),
        },
        Some(backend.clone()),
        None,
        Some(backend.clone()),
        Some(backend),
    ))
    .await;
    credential(&s, &f);
    let sbx = f.sandbox.to_string();
    let op = f.operation.to_string();
    assert_eq!(
        ok(&s, language, "getSandbox", json!({"sandbox_id":sbx})).await["observed_state"],
        "running"
    );
    assert_eq!(
        ok(&s, language, "listSandboxes", json!({"limit":1})).await["items"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        ok(&s, language, "listOperations", json!({"sandbox_id":sbx})).await["items"][0]["operation_id"],
        op
    );
    assert_eq!(
        ok(&s, language, "getOperation", json!({"operation_id":op})).await["status"],
        "succeeded"
    );
    assert_eq!(
        ok(&s, language, "wait", json!({"operation_id":op,"seconds":1})).await["status"],
        "succeeded"
    );
    let args = json!({"operation_id":op,"idempotency_key":"sdk-cancel-persistent","body":{}});
    let cancel = ok(&s, language, "cancelCommand", args.clone()).await;
    assert_ne!(cancel["operation_id"], op);
    assert_eq!(cancel, ok(&s, language, "cancelCommand", args).await);
    let output = ok(
        &s,
        language,
        "readOutput",
        json!({"operation_id":op,"output_name":"stdout"}),
    )
    .await;
    assert_eq!(output["data_base64"], "YQBi/w==");
    assert_eq!(output["truncated"], true);
    let events = ok(&s, language, "streamOutput", json!({"operation_id":op})).await;
    assert_eq!(events.as_array().unwrap().last().unwrap()["event"], "end");
    let resumed = ok(
        &s,
        language,
        "streamOutput",
        json!({"operation_id":op,"cursor":events[0]["cursor"]}),
    )
    .await;
    assert_eq!(resumed.as_array().unwrap().last().unwrap()["event"], "end");
    let capture = ok(
        &s,
        language,
        "captureFile",
        json!({"sandbox_id":sbx,"body":{"path":"hello.bin"}}),
    )
    .await;
    let args = json!({"sandbox_id":sbx,"x_file_capture":capture["capture"]});
    assert_eq!(
        ok(&s, language, "readCapturedFile", args.clone()).await["data_base64"],
        "aGVsbG8A/w=="
    );
    ok(&s, language, "releaseFileCapture", args).await;
    let dest = s.directory.path().join("download.bin");
    let receipt = ok(
        &s,
        language,
        "download",
        json!({"sandbox_id":sbx,"path":"hello.bin","destination":dest}),
    )
    .await;
    assert_eq!(receipt["release_confirmed"], true);
    assert_eq!(std::fs::read(&dest).unwrap(), b"hello\0\xff");
    assert_eq!(
        run(
            &s,
            language,
            "download",
            json!({"sandbox_id":sbx,"path":"hello.bin","destination":dest})
        )
        .await["error"],
        "file"
    );
    assert_eq!(std::fs::read(&dest).unwrap(), b"hello\0\xff");
    let args = json!({"sandbox_id":sbx,"idempotency_key":"sdk-upload-persistent","path":"hello.bin","x_file_size":7,"x_file_sha256":hex::encode(sha2::Sha256::digest(b"hello\0\xff")),"body_base64":"aGVsbG8A/w=="});
    let upload = ok(&s, language, "uploadFile", args.clone()).await;
    assert_eq!(upload, ok(&s, language, "uploadFile", args).await);
    // A separate sandbox avoids bypassing unresolved upload admission.
    let g = Fixture::new(&pool).await;
    g.finish().await;
    canonical(&pool, &g, &digest).await;
    credential(&s, &g);
    let args = json!({"sandbox_id":g.sandbox.to_string(),"idempotency_key":"sdk-execute-persistent","body":{"argv":["/bin/true"],"deadline_unix_ms":(time::OffsetDateTime::now_utc().unix_timestamp_nanos()/1_000_000) as i64+60000}});
    let execution = ok(&s, language, "executeCommand", args.clone()).await;
    assert_eq!(execution, ok(&s, language, "executeCommand", args).await);
    assert_eq!(
        run(
            &s,
            language,
            "wait",
            json!({"operation_id":execution["operation_id"],"seconds":0.1})
        )
        .await["error"],
        "wait_timeout"
    );
    assert_eq!(
        ok(
            &s,
            language,
            "getOperation",
            json!({"operation_id":execution["operation_id"]})
        )
        .await["status"],
        "queued"
    );
    ok(&s,language,"destroySandbox",json!({"sandbox_id":g.sandbox.to_string(),"idempotency_key":"sdk-destroy-persistent","body":{}})).await;
    let mut args = json!({"idempotency_key":"sdk-create-persistent","body":{"image_digest":digest,"resources":{"vcpu":1,"memory_mib":128,"disk_mib":64}}});
    let created = ok(&s, language, "createSandbox", args.clone()).await;
    assert_eq!(
        created,
        ok(&s, language, "createSandbox", args.clone()).await
    );
    args["body"]["name"] = "changed".into();
    assert_eq!(
        run(&s, language, "createSandbox", args).await["status"],
        409
    );
    assert_eq!(
        run(&s, language, "getSandbox", json!({"sandbox_id":sbx})).await["status"],
        404
    );
}
use sha2::Digest;
