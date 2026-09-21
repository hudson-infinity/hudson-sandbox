#![allow(clippy::unwrap_used, clippy::expect_used)]
use super::*;
use crate::tests::TESTS;
use object_store::ObjectStoreExt;
use sandbox_protocol::{
    AllocationId, HostId, Id, OperationId, ProjectId, SandboxId, files::Upload,
};
pub(crate) fn owner() -> ReadScope {
    ReadScope {
        version: 1,
        project_id: ProjectId::generate(),
        sandbox_id: SandboxId::generate(),
        allocation_id: AllocationId::generate(),
        host_id: HostId::generate(),
        host_epoch: 1,
        generation: 1,
    }
}
pub(crate) fn plan(bytes: &[u8]) -> SourcePlan {
    SourcePlan {
        version: 1,
        owner: owner(),
        upload: Upload {
            operation_id: OperationId::generate(),
            path: "private-customer-path".into(),
            size: bytes.len() as u64,
            sha256: Sha256::digest(bytes).into(),
            mode: 0o644,
        },
        source_attempt: OperationId::generate(),
        created_unix_ms: 1000,
        write_expires_unix_ms: 1500,
        expires_unix_ms: 2000,
        delete_after_unix_ms: 3000,
    }
}
pub(crate) fn memory() -> SourceStore {
    SourceStore {
        store: crate::tests::memory(),
    }
}

#[tokio::test]
async fn binary_empty_maximum_and_recovery_after_write_expiry() {
    let _test = TESTS.lock().await;
    let store = memory();
    for bytes in [
        vec![],
        b"\0\xffprivate-file-bytes".to_vec(),
        vec![0xfe; sandbox_protocol::files::MAX_FILE_BYTES as usize],
    ] {
        let p = plan(&bytes);
        let (a, b) = tokio::join!(
            store.upload(&p, &p.owner, 1000, &bytes),
            store.upload(&p, &p.owner, 1000, &bytes)
        );
        let reference = a.unwrap();
        assert_eq!(reference, b.unwrap());
        assert_eq!(
            store.upload(&p, &p.owner, 1500, &bytes).await,
            Err(Error::Expired)
        );
        let recovered = store.reconcile(&p, &p.owner, 1501).await.unwrap();
        assert_eq!(recovered, reference);
        let body = store.read(&reference, &p.owner, 1999).await.unwrap();
        assert_eq!(body.bytes, bytes);
        assert!(!format!("{body:?} {reference:?}").contains("private-file-bytes"));
        assert!(!format!("{reference:?}").contains(&p.upload.path));
        assert!(matches!(
            store.read(&reference, &p.owner, 2000).await,
            Err(Error::Expired)
        ));
        assert_eq!(
            store.reconcile(&p, &owner(), 1501).await,
            Err(Error::OwnerMismatch)
        );
    }
}
#[test]
fn descriptors_bind_every_field_but_never_use_paths_as_object_keys() {
    let p = plan(b"x");
    let digest = p.metadata_digest().unwrap();
    let key = p.object_key().unwrap();
    assert!(key.starts_with("file-source/v1/"));
    assert!(!key.contains(&p.upload.path));
    for field in 0..16 {
        let mut changed = p.clone();
        match field {
            0 => changed.owner.project_id = ProjectId::generate(),
            1 => changed.owner.sandbox_id = SandboxId::generate(),
            2 => changed.upload.operation_id = OperationId::generate(),
            3 => changed.owner.allocation_id = AllocationId::generate(),
            4 => changed.owner.host_id = HostId::generate(),
            5 => changed.owner.host_epoch += 1,
            6 => changed.owner.generation += 1,
            7 => changed.source_attempt = OperationId::generate(),
            8 => changed.upload.path = "other".into(),
            9 => changed.upload.size += 1,
            10 => changed.upload.sha256 = [0; 32],
            11 => changed.upload.mode = 0o755,
            12 => changed.created_unix_ms -= 1,
            13 => changed.write_expires_unix_ms -= 1,
            14 => changed.expires_unix_ms += 1,
            _ => changed.delete_after_unix_ms += 1,
        }
        assert_ne!(digest, changed.metadata_digest().unwrap());
    }
    for field in 0..8 {
        let mut changed = p.clone();
        match field {
            0 => changed.version = 2,
            1 => changed.owner.version = 2,
            2 => changed.owner.generation = 0,
            3 => changed.upload.path = "../bad".into(),
            4 => changed.upload.size = sandbox_protocol::files::MAX_FILE_BYTES + 1,
            5 => changed.write_expires_unix_ms = 1000,
            6 => changed.expires_unix_ms = 1499,
            _ => changed.delete_after_unix_ms = 1999,
        }
        assert!(changed.validate().is_err());
    }
    let mut json = serde_json::to_value(p).unwrap();
    json["bucket"] = "customer".into();
    assert!(serde_json::from_value::<SourcePlan>(json).is_err());
}
#[tokio::test]
async fn changed_metadata_or_bytes_cannot_replace_a_retained_source() {
    let _test = TESTS.lock().await;
    let store = memory();
    let p = plan(b"abc");
    let original = store.upload(&p, &p.owner, 1000, b"abc").await.unwrap();
    for field in 0..5 {
        let mut changed = p.clone();
        match field {
            0 => changed.upload.path = "another".into(),
            1 => changed.upload.mode = 0o755,
            2 => changed.write_expires_unix_ms += 1,
            3 => changed.expires_unix_ms += 1,
            _ => changed.delete_after_unix_ms += 1,
        }
        assert_eq!(
            store.upload(&changed, &changed.owner, 1000, b"abc").await,
            Err(Error::Conflict)
        );
    }
    assert_eq!(
        store.upload(&p, &p.owner, 1000, b"xyz").await,
        Err(Error::Corrupt)
    );
    assert_eq!(
        store.upload(&p, &p.owner, 1000, b"abcd").await,
        Err(Error::InvalidMetadata)
    );
    assert_eq!(
        store.read(&original, &p.owner, 1000).await.unwrap().bytes,
        b"abc"
    );
    let mut bad = original.clone();
    bad.etag = "changed".into();
    assert!(matches!(
        store.read(&bad, &p.owner, 1000).await,
        Err(Error::Corrupt)
    ));
    bad.etag = "\r\n".into();
    assert!(matches!(
        store.read(&bad, &p.owner, 1000).await,
        Err(Error::InvalidMetadata)
    ));
    store
        .store
        .inner
        .put(&Path::from(p.object_key().unwrap()), b"xyz".to_vec().into())
        .await
        .unwrap();
    assert_eq!(
        store.reconcile(&p, &p.owner, 1000).await,
        Err(Error::Corrupt)
    );
}
#[tokio::test]
async fn truncated_extended_and_interrupted_bodies_never_release_prefixes() {
    let _test = TESTS.lock().await;
    let store = memory();
    let p = plan(b"complete");
    let reference = store.upload(&p, &p.owner, 1000, b"complete").await.unwrap();
    for bytes in [b"com".to_vec(), b"completeEXTRA".to_vec()] {
        let mut response = store
            .store
            .inner
            .get(&Path::from(p.object_key().unwrap()))
            .await
            .unwrap();
        response.payload = object_store::GetResultPayload::Stream(Box::pin(
            futures_util::stream::iter(vec![Ok(bytes.into())]),
        ));
        assert_eq!(
            SourceStore::verify(&p, Some(&reference), response).await,
            Err(Error::Corrupt)
        );
    }
    let mut response = store
        .store
        .inner
        .get(&Path::from(p.object_key().unwrap()))
        .await
        .unwrap();
    response.payload =
        object_store::GetResultPayload::Stream(Box::pin(futures_util::stream::iter(vec![
            Ok(b"com".to_vec().into()),
            Err(object_store::Error::Generic {
                store: "test",
                source: "private-error".into(),
            }),
        ])));
    assert_eq!(
        SourceStore::verify(&p, Some(&reference), response).await,
        Err(Error::Unavailable)
    );
}
#[tokio::test]
async fn sources_share_the_process_transfer_budget_with_outputs() {
    let _test = TESTS.lock().await;
    let store = memory();
    let p = plan(b"x");
    let reference = store.upload(&p, &p.owner, 1000, b"x").await.unwrap();
    let permits = TRANSFERS.try_acquire_many(4).unwrap();
    assert_eq!(
        store.upload(&p, &p.owner, 1000, b"x").await,
        Err(Error::Busy)
    );
    assert_eq!(store.reconcile(&p, &p.owner, 1000).await, Err(Error::Busy));
    assert!(matches!(
        store.read(&reference, &p.owner, 1000).await,
        Err(Error::Busy)
    ));
    drop(permits);
    assert_eq!(
        store.read(&reference, &p.owner, 1000).await.unwrap().bytes,
        b"x"
    );
}

#[tokio::test]
#[ignore = "requires private unversioned and versioned MinIO buckets"]
async fn output_minio_file_sources_conditional_upload_reconnect_and_pinned_versions() {
    let _test = TESTS.lock().await;
    let config = |versioned| crate::S3Config {
        endpoint: std::env::var("HUDSON_TEST_S3_ENDPOINT").unwrap(),
        region: "us-east-1".into(),
        bucket: std::env::var(if versioned {
            "HUDSON_TEST_S3_VERSIONED_BUCKET"
        } else {
            "HUDSON_TEST_S3_BUCKET"
        })
        .unwrap(),
        access_key: std::env::var("HUDSON_TEST_S3_ACCESS_KEY").unwrap(),
        secret_key: std::env::var("HUDSON_TEST_S3_SECRET_KEY").unwrap(),
        session_token: None,
        allow_loopback_http: true,
    };
    for versioned in [false, true] {
        let store = config(versioned).build_sources().unwrap();
        let cleaner = config(versioned).build_source_retirer().unwrap();
        for bytes in [vec![], (0..65539).map(|n| (n % 251) as u8).collect()] {
            let p = plan(&bytes);
            let (a, b) = tokio::join!(
                store.upload(&p, &p.owner, 1000, &bytes),
                store.upload(&p, &p.owner, 1000, &bytes)
            );
            let reference = a.unwrap();
            assert_eq!(reference, b.unwrap());
            let reconnect = config(versioned).build_sources().unwrap();
            assert_eq!(
                reconnect.reconcile(&p, &p.owner, 1501).await.unwrap(),
                reference
            );
            let body = reconnect.read(&reference, &p.owner, 1501).await.unwrap();
            assert_eq!(body.bytes, bytes);
            assert_eq!(Sha256::digest(&body.bytes).as_slice(), p.upload.sha256);
            let mut changed = p.clone();
            changed.upload.mode = 0o755;
            assert_eq!(
                store.upload(&changed, &p.owner, 1000, &bytes).await,
                Err(Error::Conflict)
            );
            let receipt = cleaner
                .retire(&p, &p.owner, Some(&reference), 3000)
                .await
                .unwrap();
            assert_eq!(
                cleaner
                    .retire(&p, &p.owner, Some(&reference), 4000)
                    .await
                    .unwrap(),
                receipt
            );
            assert!(matches!(
                store.read(&reference, &p.owner, 1500).await,
                Err(Error::Missing | Error::Corrupt)
            ));
            assert_eq!(
                store.upload(&p, &p.owner, 1000, &bytes).await,
                Err(Error::Conflict)
            );
            eprintln!(
                "file_source_storage_observation={}",
                serde_json::json!({"versioned":versioned,"bytes":bytes.len(),"verified_full_sha256":true,"same_attempt_recovered":true,"retirement_confirmed":true,"late_upload_rejected":true})
            );
            if let Some(version) = &receipt.marker_version {
                cleaner.delete.delete(&p, version).await.unwrap();
            } else {
                store
                    .store
                    .inner
                    .delete(&Path::from(p.object_key().unwrap()))
                    .await
                    .unwrap();
            }
        }
    }
}
