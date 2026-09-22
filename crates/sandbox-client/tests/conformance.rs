//! One independent wire corpus exercised through all three public clients.
#![allow(clippy::unwrap_used, clippy::expect_used)]
#[path = "support/dispatch.rs"]
mod dispatch;
#[path = "support/external.rs"]
mod external;
#[path = "support/server.rs"]
mod server;
use axum::{
    Router,
    body::{Body, Bytes},
    extract::State,
    http::{HeaderMap, Method, Uri},
    response::Response,
};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
};
#[derive(Clone)]
struct Cases {
    requests: Arc<Vec<Value>>,
    seen: Arc<Mutex<Vec<Value>>>,
}
async fn serve(
    State(state): State<Cases>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let query: std::collections::BTreeMap<String, String> =
        url::form_urlencoded::parse(uri.query().unwrap_or("").as_bytes())
            .into_owned()
            .collect();
    let raw = headers
        .get("content-type")
        .is_some_and(|v| v == "application/octet-stream");
    let parsed = if body.is_empty() {
        Value::Null
    } else if raw {
        json!({"base64":STANDARD.encode(&body)})
    } else {
        serde_json::from_slice(&body).unwrap()
    };
    let actual = json!({"method":method.as_str(),"path":uri.path(),"query":query,"headers":headers.iter().map(|(k,v)|(k.as_str().to_owned(),v.to_str().unwrap().to_owned())).collect::<std::collections::BTreeMap<_,_>>(),"body":parsed});
    let index = {
        let mut seen = state.seen.lock().unwrap();
        let index = seen.len();
        seen.push(actual);
        index
    };
    let Some(expected) = state.requests.get(index) else {
        return Response::builder().status(500).body(Body::empty()).unwrap();
    };
    let reply = &expected["response"];
    let mut response = Response::builder().status(reply["status"].as_u64().unwrap() as u16);
    for (name, value) in reply["headers"].as_object().unwrap() {
        response = response.header(name, value.as_str().unwrap());
    }
    let body = if reply["lost"] == true {
        Body::from_stream(tokio_stream::iter([
            Ok::<_, std::io::Error>("{"),
            Err(std::io::Error::other("lost reply")),
        ]))
    } else if reply["stall"] == true {
        Body::from_stream(tokio_stream::pending::<Result<&'static str, std::io::Error>>())
    } else if let Some(repeat) = reply["repeat_byte"].as_array() {
        Body::from(vec![
            repeat[0].as_u64().unwrap() as u8;
            repeat[1].as_u64().unwrap() as usize
        ])
    } else if let Some(binary) = reply["body_base64"].as_str() {
        Body::from(STANDARD.decode(binary).unwrap())
    } else if let Some(raw) = reply["raw"].as_str() {
        Body::from(raw.to_owned())
    } else {
        Body::from(serde_json::to_vec(&reply["body"]).unwrap())
    };
    response.body(body).unwrap()
}
fn normalize(result: Result<Value, sandbox_client::Error>) -> Value {
    match result {
        Ok(value) => json!({"ok":value}),
        Err(error) => {
            assert!(!format!("{error:?} {error}").contains("private-backend-text"));
            if let sandbox_client::Error::Http { status, .. } = &error {
                json!({"error":"http","status":status,"code":error.problem_code(),"operation_id":error.operation_id()})
            } else {
                json!({"error":error.kind()})
            }
        }
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rust_python_typescript_share_https_conformance() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let cases: Vec<Value> =
        serde_json::from_slice(&std::fs::read(root.join("api/conformance/clients.json")).unwrap())
            .unwrap();
    for language in ["rust", "python", "typescript"] {
        for original in &cases {
            let mut case = original.clone();
            let name = case["name"].as_str().unwrap().to_owned();
            let state = Cases {
                requests: Arc::new(case["requests"].as_array().unwrap().clone()),
                seen: Arc::default(),
            };
            let server =
                server::Server::start(Router::new().fallback(serve).with_state(state.clone()))
                    .await;
            match case["config_variant"].as_str() {
                Some("wrong_hostname") => server.edit_config(|v| {
                    v["endpoint"] = server.endpoint.replace("localhost", "127.0.0.1").into()
                }),
                Some("missing_ca") => server.edit_config(|v| {
                    v.as_object_mut().unwrap().remove("ca_file");
                }),
                None => {}
                _ => panic!("unknown config variant"),
            }
            let destination = server.directory.path().join("download.bin");
            if case["action"] == "download" {
                case["args"]["destination"] = destination.to_str().unwrap().into();
                if case["file_check"]["exists_before"] == true {
                    server::private(&destination, b"original");
                }
            }
            let actual = if language == "rust" {
                normalize(dispatch::dispatch(&server.client(), &case).await)
            } else {
                external::run(language, &server.config, &case).await
            };
            assert_eq!(actual, case["expected"], "{language}: {name}");
            if case["action"] == "download" {
                use sha2::Digest;
                if let Some(sha) = case["file_check"]["sha256"].as_str() {
                    let bytes = std::fs::read(&destination).unwrap();
                    assert_eq!(
                        hex::encode(sha2::Sha256::digest(&bytes)),
                        sha,
                        "{language}: {name}"
                    );
                    assert_eq!(
                        bytes.len() as u64,
                        case["file_check"]["size"].as_u64().unwrap()
                    );
                } else {
                    assert!(!destination.exists(), "{language}: {name}");
                }
                assert_eq!(
                    std::fs::read_dir(server.directory.path()).unwrap().count(),
                    3 + usize::from(destination.exists()),
                    "{language}: leaked staging file"
                );
            }
            let seen = state.seen.lock().unwrap();
            assert_eq!(
                seen.len(),
                state.requests.len(),
                "{language}: {name}: missing request or implicit retry"
            );
            for (actual, expected) in seen.iter().zip(state.requests.iter()) {
                for field in ["method", "path", "query", "body"] {
                    assert_eq!(
                        actual[field], expected[field],
                        "{language}: {name}: {field}"
                    );
                }
                assert_eq!(
                    actual["headers"]["authorization"],
                    "Bearer test-private-credential"
                );
                for (key, value) in expected["headers"].as_object().unwrap() {
                    assert_eq!(
                        &actual["headers"][key], value,
                        "{language}: {name}: header {key}"
                    );
                }
            }
        }
    }
}
