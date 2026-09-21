use super::*;
use axum::{
    Router,
    body::{Body, to_bytes},
};
use http::{Request, StatusCode};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tower::ServiceExt;

async fn request(
    app: &Router,
    bearer: &str,
    sandbox: &str,
    method: &str,
    capture: &str,
    offset: u64,
) -> (StatusCode, http::HeaderMap, Vec<u8>) {
    let request = Request::builder()
        .method(method)
        .uri(format!(
            "/v1/sandboxes/{sandbox}/files/captures{}",
            if method == "GET" {
                format!("?offset={offset}&limit=32768")
            } else {
                String::new()
            }
        ))
        .header("authorization", format!("Bearer {bearer}"))
        .header("x-file-capture", capture)
        .body(Body::empty())
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = to_bytes(response.into_body(), 65536)
        .await
        .unwrap()
        .to_vec();
    (status, headers, bytes)
}

pub(super) async fn round_trip(
    app: &Router,
    bearer: &str,
    controller: &mut sandbox_controller::Controller,
    sandbox: &str,
    upload: bool,
) -> String {
    let mut expected = vec![0; 65536];
    expected.extend_from_slice(b"\0\xffa");
    if upload {
        upload_file(app, bearer, controller, sandbox, &expected).await;
    }
    let key = OperationId::generate().to_string();
    let command = if upload {
        "/bin/busybox cp /workspace/uploaded.bin /workspace/public.bin"
    } else {
        "/bin/busybox dd if=/dev/zero of=/workspace/public.bin bs=65536 count=1; /bin/busybox printf '\\000\\377a' >> /workspace/public.bin"
    };
    let (status, admission) = super::http(app, bearer, "POST", &format!("/v1/sandboxes/{sandbox}/execute"), &key,
        json!({"argv":["/bin/busybox","sh","-c",command],"deadline_unix_ms":guardian::wall_ms()+10000,"output_limit":1024})).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{admission}");
    let operation = admission["operation_id"].as_str().unwrap();
    let until = Instant::now() + Duration::from_secs(15);
    loop {
        controller.tick().await.unwrap();
        let (_, result) = super::http(
            app,
            bearer,
            "GET",
            &format!("/v1/operations/{operation}"),
            &key,
            Value::Null,
        )
        .await;
        if result["status"] == "succeeded" {
            break;
        }
        assert!(
            Instant::now() < until,
            "public file command failed: {result}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let (status, captured) = super::http(
        app,
        bearer,
        "POST",
        &format!("/v1/sandboxes/{sandbox}/files/captures"),
        &key,
        json!({"path":"public.bin"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{captured}");
    assert_eq!(captured["simulated"], false);
    assert_eq!(captured["guest_reported"], true);
    assert_eq!(captured["size"], expected.len());
    let capture = captured["capture"].as_str().unwrap();
    let mut bytes = Vec::new();
    loop {
        let (status, headers, chunk) =
            request(app, bearer, sandbox, "GET", capture, bytes.len() as u64).await;
        assert_eq!(
            status,
            StatusCode::OK,
            "{}",
            String::from_utf8_lossy(&chunk)
        );
        assert_eq!(headers["cache-control"], "no-store");
        assert_eq!(headers["x-file-simulated"], "false");
        assert_eq!(
            headers["x-file-sha256"].to_str().unwrap(),
            captured["sha256"].as_str().unwrap()
        );
        bytes.extend(chunk);
        assert_eq!(
            headers["x-file-next-offset"]
                .to_str()
                .unwrap()
                .parse::<usize>()
                .unwrap(),
            bytes.len()
        );
        if headers["x-file-eof"] == "true" {
            break;
        }
        assert!(bytes.len() <= expected.len());
    }
    assert_eq!(bytes, expected);
    assert_eq!(
        hex::encode(Sha256::digest(&bytes)),
        captured["sha256"].as_str().unwrap()
    );
    for _ in 0..2 {
        assert_eq!(
            request(app, bearer, sandbox, "DELETE", capture, 0).await.0,
            StatusCode::NO_CONTENT
        );
    }
    assert_eq!(
        request(app, bearer, sandbox, "GET", capture, 0).await.0,
        StatusCode::GONE
    );
    let (status, fresh) = super::http(
        app,
        bearer,
        "POST",
        &format!("/v1/sandboxes/{sandbox}/files/captures"),
        &key,
        json!({"path":"public.bin"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_ne!(fresh["capture"], captured["capture"]);
    eprintln!(
        "real_public_file_observation={}",
        json!({"bytes":bytes.len(),"full_sha256_verified":true,"public_execute":true,"public_upload":upload,"public_capture_ranges_release":true,"simulated":false})
    );
    fresh["capture"].as_str().unwrap().into()
}

pub(super) async fn after_destroy(app: &Router, bearer: &str, sandbox: &str, capture: &str) {
    let (status, _, body) = request(app, bearer, sandbox, "GET", capture, 0).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(
        serde_json::from_slice::<Value>(&body).unwrap()["code"],
        "not_found"
    );
}

async fn upload_file(
    app: &Router,
    bearer: &str,
    controller: &mut sandbox_controller::Controller,
    sandbox: &str,
    bytes: &[u8],
) {
    let key = OperationId::generate().to_string();
    let mut operation = String::new();
    for _ in 0..2 {
        let req = Request::builder()
            .method("PUT")
            .uri(format!("/v1/sandboxes/{sandbox}/files?path=uploaded.bin"))
            .header("authorization", format!("Bearer {bearer}"))
            .header("idempotency-key", &key)
            .header("content-type", "application/octet-stream")
            .header("x-file-size", bytes.len())
            .header("x-file-sha256", hex::encode(Sha256::digest(bytes)))
            .body(Body::from(bytes.to_vec()))
            .unwrap();
        let res = app.clone().oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::ACCEPTED);
        let result: Value =
            serde_json::from_slice(&to_bytes(res.into_body(), 65536).await.unwrap()).unwrap();
        if operation.is_empty() {
            operation = result["operation_id"].as_str().unwrap().into();
        } else {
            assert_eq!(result["operation_id"], operation);
        }
    }
    let until = Instant::now() + Duration::from_secs(30);
    loop {
        controller.tick().await.unwrap();
        let (_, result) = super::http(
            app,
            bearer,
            "GET",
            &format!("/v1/operations/{operation}"),
            &key,
            Value::Null,
        )
        .await;
        if result["status"] == "succeeded" {
            assert_eq!(result["phase"], "file_committed");
            assert_eq!(result["result"]["simulated"], false);
            assert_eq!(result["result"]["guest_reported"], true);
            assert_eq!(
                result["result"]["sha256"],
                hex::encode(Sha256::digest(bytes))
            );
            break;
        }
        assert!(
            Instant::now() < until,
            "public upload did not settle: {result}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}
