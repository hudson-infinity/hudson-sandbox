#![allow(clippy::unwrap_used)]
use sandbox_guest::model::{Execute, MAX_OUTPUT};
use sandbox_protocol::{Id, OperationId};
use std::collections::BTreeMap;

fn request() -> Execute {
    Execute {
        operation_id: OperationId::generate(),
        argv: vec!["/bin/echo".into(), "secret-value".into()],
        env: BTreeMap::from([("TOKEN".into(), "private-value".into())]),
        cwd: "/".into(),
        deadline_unix_ms: 1,
        output_limit: 1024,
    }
}
#[test]
fn digest_covers_payload_and_normalizes_environment_order() {
    let mut a = request();
    a.env.insert("A".into(), "1".into());
    let mut b = a.clone();
    b.env = BTreeMap::from([
        ("A".into(), "1".into()),
        ("TOKEN".into(), "private-value".into()),
    ]);
    assert_eq!(a.digest().unwrap(), b.digest().unwrap());
    b.argv.push("changed".into());
    assert_ne!(a.digest().unwrap(), b.digest().unwrap());
    b = a.clone();
    b.deadline_unix_ms += 1;
    assert_ne!(a.digest().unwrap(), b.digest().unwrap());
    b = a.clone();
    b.output_limit += 1;
    assert_ne!(a.digest().unwrap(), b.digest().unwrap());
    b = a.clone();
    b.cwd = "/tmp".into();
    assert_ne!(a.digest().unwrap(), b.digest().unwrap());
    b = a.clone();
    b.env.insert("TOKEN".into(), "changed".into());
    assert_ne!(a.digest().unwrap(), b.digest().unwrap());
}
#[test]
fn debug_does_not_disclose_command_or_environment() {
    let text = format!("{:?}", request());
    assert!(!text.contains("secret-value"));
    assert!(!text.contains("private-value"));
    assert!(!text.contains("/bin/echo"));
}
#[test]
fn rejects_invalid_arguments_environment_and_limits() {
    let a = request();
    a.validate().unwrap();
    let mut b = a.clone();
    b.argv.clear();
    assert!(b.validate().is_err());
    let mut b = a.clone();
    b.argv[0] = "a\0b".into();
    assert!(b.validate().is_err());
    let mut b = a.clone();
    b.env.insert("1KEY".into(), "value".into());
    assert!(b.validate().is_err());
    let mut b = a.clone();
    b.env.insert("A".into(), "a\0b".into());
    assert!(b.validate().is_err());
    let mut b = a.clone();
    b.cwd = "relative".into();
    assert!(b.validate().is_err());
    let mut b = a.clone();
    b.output_limit = MAX_OUTPUT + 1;
    assert!(b.validate().is_err());
    let mut b = a.clone();
    b.output_limit = 0;
    assert!(b.validate().is_err());
    let mut b = a.clone();
    b.argv = vec!["x".repeat(32769)];
    assert!(b.validate().is_err());
    let mut b = a;
    b.argv = vec!["\u{1}".repeat(20000)];
    assert!(b.validate().is_err());
}
#[test]
fn rejects_unknown_fields() {
    let mut value = serde_json::to_value(request()).unwrap();
    value["shell"] = true.into();
    assert!(serde_json::from_value::<Execute>(value).is_err());
}
