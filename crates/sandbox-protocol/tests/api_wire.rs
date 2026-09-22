//! Golden normalization is part of the durable idempotency contract.
#![allow(clippy::unwrap_used)]
use sandbox_protocol::{RequestDigest, api::*};
use serde_json::{Value, json};
#[test]
fn generated_requests_keep_original_normalization_and_digests() {
    let raw = json!({"image_digest":format!("sha256:{}","a".repeat(64)),"resources":{"vcpu":1,"memory_mib":128,"disk_mib":64},"ignored_by_original_create":true});
    let actual: CreateRequest = serde_json::from_value(raw).unwrap();
    let original = json!({"image_digest":format!("sha256:{}","a".repeat(64)),"name":null,"resources":{"vcpu":1,"memory_mib":128,"disk_mib":64},"correlation_id":null});
    assert_eq!(serde_json::to_value(&actual).unwrap(), original);
    assert_eq!(
        RequestDigest::compute("POST", "/v1/sandboxes", &actual).unwrap(),
        RequestDigest::compute("POST", "/v1/sandboxes", &original).unwrap()
    );
    let actual: CommandInput = serde_json::from_value(
        json!({"argv":["secret-command"],"env":{"Z":"private","A":"first"},"deadline_unix_ms":1}),
    )
    .unwrap();
    // Command::digest hashes serialized fields, so both defaults and field order matter.
    assert_eq!(
        serde_json::to_string(&actual).unwrap(),
        r#"{"argv":["secret-command"],"env":{"A":"first","Z":"private"},"cwd":"/","deadline_unix_ms":1,"output_limit":1048576}"#
    );
    assert!(!format!("{actual:?}").contains("secret"));
    assert!(!format!("{actual:?}").contains("private"));
    assert_eq!(
        serde_json::to_value(serde_json::from_value::<DestroyRequest>(json!({})).unwrap()).unwrap(),
        json!({"correlation_id":null})
    );
    assert!(serde_json::from_value::<DestroyRequest>(json!({"unexpected":1})).is_err());
    assert!(serde_json::from_value::<CancelRequest>(json!({"unexpected":1})).is_err());
}
#[test]
fn generated_response_optional_fields_preserve_wire_shape() {
    let list: SandboxList = serde_json::from_value(json!({"items":[],"next_cursor":null})).unwrap();
    assert_eq!(
        serde_json::to_value(list).unwrap(),
        json!({"items":[],"next_cursor":null})
    );
    assert!(serde_json::from_value::<SandboxList>(json!({"items":[]})).is_err());
    let raw = json!({"operation_id":"op_fixture","sandbox_id":"sbx_fixture","kind":"execute","status":"queued","created_at":"time"});
    let body: OperationBody = serde_json::from_value(raw.clone()).unwrap();
    assert_eq!(serde_json::to_value(body).unwrap(), raw);
    let p: ProblemBody =
        serde_json::from_value(json!({"title":"No such resource","status":404,"code":"not_found"}))
            .unwrap();
    let p: Value = serde_json::to_value(p).unwrap();
    assert!(p.get("operation_id").is_none());
}
