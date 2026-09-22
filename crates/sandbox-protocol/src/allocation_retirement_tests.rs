#![allow(clippy::unwrap_used)]
use super::*;
use crate::{AllocationId, HostId, ProjectId, SandboxId};

fn intent() -> Intent {
    Intent {
        version: 1,
        retirement: OperationId::generate(),
        permit: Permit {
            host: HostId::generate(),
            project: ProjectId::generate(),
            sandbox: SandboxId::generate(),
            allocation: AllocationId::generate(),
            create_operation: OperationId::generate(),
            generation: 3,
            original_epoch: 2,
            serial: 71,
        },
        commands: DomainClosure::Retired {
            through: OperationId::generate(),
        },
        files: DomainClosure::Empty {},
        release_evidence_sha256: "ab".repeat(32),
        simulated: false,
    }
}
fn request(i: &Intent) -> Request {
    Request {
        intent: i.clone(),
        reporting_epoch: 2,
        revision: 1,
        expires_unix_ms: 2000,
    }
}
#[test]
fn scope_survives_renewed_claim_and_reporting_epoch() {
    let i = intent();
    assert_eq!(Intent::decode(&i.encode().unwrap()).unwrap(), i);
    let r = request(&i);
    r.validate(&i, 2, 1, 1000).unwrap();
    let mut retry = r.clone();
    retry.revision = 2;
    retry.reporting_epoch = 3;
    retry.expires_unix_ms = 3000;
    retry.validate(&i, 3, 2, 2000).unwrap();
    assert_eq!(Request::decode(&retry.encode().unwrap()).unwrap(), retry);
    assert_eq!(retry.intent.digest().unwrap(), i.digest().unwrap());
    assert_eq!(r.validate(&i, 3, 2, 2000), Err(Error::Claim));
}
#[test]
fn every_frozen_field_is_part_of_retry_identity() {
    let i = intent();
    let mutations: [fn(&mut Intent); 13] = [
        |x| x.retirement = OperationId::generate(),
        |x| x.permit.host = HostId::generate(),
        |x| x.permit.project = ProjectId::generate(),
        |x| x.permit.sandbox = SandboxId::generate(),
        |x| x.permit.allocation = AllocationId::generate(),
        |x| x.permit.create_operation = OperationId::generate(),
        |x| x.permit.generation += 1,
        |x| x.permit.original_epoch += 1,
        |x| x.permit.serial += 1,
        |x| x.commands = DomainClosure::Empty {},
        |x| {
            x.files = DomainClosure::Retired {
                through: OperationId::generate(),
            }
        },
        |x| x.release_evidence_sha256 = "cd".repeat(32),
        |x| x.simulated = true,
    ];
    for mutate in mutations {
        let mut changed = request(&i);
        mutate(&mut changed.intent);
        assert_eq!(changed.validate(&i, 2, 1, 1000), Err(Error::Scope));
        assert_ne!(changed.intent.digest().unwrap(), i.digest().unwrap());
    }
}
#[test]
fn parsing_rejects_missing_ambiguous_and_unbounded_scope() {
    let i = intent();
    let bytes = i.encode().unwrap();
    for field in ["commands", "files", "simulated", "release_evidence_sha256"] {
        let mut v = serde_json::to_value(&i).unwrap();
        v.as_object_mut().unwrap().remove(field);
        assert!(Intent::decode(&serde_json::to_vec(&v).unwrap()).is_err());
    }
    let text = String::from_utf8(bytes.clone()).unwrap();
    let duplicate = text.replacen("{", "{\"version\":1,", 1);
    assert!(Intent::decode(duplicate.as_bytes()).is_err());
    let mut v = serde_json::to_value(&i).unwrap();
    v["files"] = serde_json::json!({"kind":"empty","through":OperationId::generate()});
    assert!(Intent::decode(&serde_json::to_vec(&v).unwrap()).is_err());
    v["files"] = serde_json::json!({"kind":"retired"});
    assert!(Intent::decode(&serde_json::to_vec(&v).unwrap()).is_err());
    let mut oversized = bytes;
    oversized.resize(MAX_BYTES + 1, b' ');
    assert!(Intent::decode(&oversized).is_err());
    assert!(Request::decode(&oversized).is_err());
}
#[test]
fn malformed_identities_versions_and_digests_fail() {
    let i = intent();
    for bad in ["", "AA", &"A".repeat(64), &"g".repeat(64), &"0".repeat(63)] {
        let mut x = i.clone();
        x.release_evidence_sha256 = bad.into();
        assert_eq!(x.validate(), Err(Error::Invalid));
    }
    let mut x = i.clone();
    x.version = 2;
    assert!(x.encode().is_err());
    x = i.clone();
    x.retirement = OperationId::from_uuid(uuid::Uuid::nil());
    assert!(x.encode().is_err());
    x = i.clone();
    x.permit.serial = 0;
    assert!(x.encode().is_err());
    x = i.clone();
    x.commands = DomainClosure::Retired {
        through: x.retirement,
    };
    assert!(x.encode().is_err());
    x = i.clone();
    x.permit.generation = 0;
    assert!(x.encode().is_err());
}
#[test]
fn expired_and_stale_claims_are_not_refreshed_by_parsing() {
    let i = intent();
    let r = request(&i);
    for (epoch, revision, now) in [
        (1, 1, 1000),
        (3, 1, 1000),
        (2, 2, 1000),
        (2, 0, 1000),
        (2, 1, 2000),
        (2, 1, 0),
    ] {
        assert_eq!(r.validate(&i, epoch, revision, now), Err(Error::Claim));
    }
    let mut invalid = r;
    invalid.revision = 0;
    let parsed = Request::decode(&invalid.encode().unwrap()).unwrap();
    assert_eq!(parsed.validate(&i, 2, 1, 1000), Err(Error::Claim));
}

#[test]
fn digest_ignores_json_key_order_but_not_domain_scope() {
    let i = intent();
    // Value serializes object keys in sorted order, unlike the typed struct.
    let reordered = serde_json::to_vec_pretty(&serde_json::to_value(&i).unwrap()).unwrap();
    assert_ne!(reordered, i.encode().unwrap());
    assert_eq!(
        Intent::decode(&reordered).unwrap().digest().unwrap(),
        i.digest().unwrap()
    );
    let mut unknown = serde_json::to_value(request(&i)).unwrap();
    unknown["intent"]["permit"]["extra"] = serde_json::json!(true);
    assert!(Request::decode(&serde_json::to_vec(&unknown).unwrap()).is_err());
    let text = String::from_utf8(request(&i).encode().unwrap()).unwrap();
    let duplicate = text.replacen("{", "{\"revision\":1,", 1);
    assert!(Request::decode(duplicate.as_bytes()).is_err());
}
