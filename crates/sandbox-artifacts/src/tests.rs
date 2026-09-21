#![allow(clippy::unwrap_used, clippy::expect_used)]
use super::*;
use object_store::ObjectStoreExt;
use sandbox_protocol::{
    AllocationId, HostId, Id, OperationId, ProjectId, SandboxId,
    command::MAX_OUTPUT,
    output::{OutputName, OutputRefs},
};

// Tests share the production process-wide admission semaphore; serialize their
// fixtures, except for the explicit concurrency test within one fixture.
static TESTS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
fn owner() -> OutputOwner {
    OutputOwner {
        project_id: ProjectId::generate(),
        sandbox_id: SandboxId::generate(),
        operation_id: OperationId::generate(),
        allocation_id: AllocationId::generate(),
        generation: 1,
        host_id: HostId::generate(),
        host_epoch: 1,
        boot_id: "guest-boot-one".into(),
    }
}
fn plan(bytes: &[u8]) -> OutputPlan {
    OutputPlan {
        version: 1,
        owner: owner(),
        upload_attempt: OperationId::generate(),
        name: OutputName::Stdout,
        size: bytes.len() as u64,
        sha256: hex::encode(Sha256::digest(bytes)),
        seen: bytes.len() as u64,
        truncated: false,
        created_unix_ms: 1000,
        expires_unix_ms: 2000,
        delete_after_unix_ms: 3000,
    }
}
fn memory() -> ArtifactStore {
    ArtifactStore {
        inner: Arc::new(object_store::memory::InMemory::new()),
    }
}

#[tokio::test]
async fn binary_ranges_empty_output_and_truncation() {
    let _test = TESTS.lock().await;
    let store = memory();
    let bytes = b"\x00\xff\xfehello\n";
    let mut p = plan(bytes);
    p.seen += 100;
    p.truncated = true;
    let r = store.upload(&p, &p.owner, 1000, bytes).await.unwrap();
    let a = store.read(&r, &p.owner, 1500, 0, 3).await.unwrap();
    assert_eq!(a.bytes, bytes[..3]);
    assert!(!a.eof);
    assert!(a.truncated);
    let b = store
        .read(&r, &p.owner, 1500, a.next_offset, MAX_CHUNK)
        .await
        .unwrap();
    assert_eq!(b.bytes, bytes[3..]);
    assert!(b.eof);
    assert!(!format!("{b:?}").contains("hello"));
    assert!(
        store
            .read(&r, &p.owner, 1500, p.size, 1)
            .await
            .unwrap()
            .bytes
            .is_empty()
    );
    for (offset, limit) in [(p.size + 1, 1), (u64::MAX, 1), (0, 0), (0, MAX_CHUNK + 1)] {
        assert!(matches!(
            store.read(&r, &p.owner, 1500, offset, limit).await,
            Err(Error::Bounds)
        ));
    }
    let mut empty = plan(b"");
    empty.name = OutputName::Stderr;
    let r = store.upload(&empty, &empty.owner, 1000, b"").await.unwrap();
    let chunk = store.read(&r, &empty.owner, 1000, 0, 1).await.unwrap();
    assert!(chunk.eof && chunk.bytes.is_empty() && !chunk.truncated);
}

#[tokio::test]
async fn lost_ack_retry_verifies_existing_bytes_and_rejects_changed_plan() {
    let _test = TESTS.lock().await;
    let store = memory();
    let p = plan(b"first");
    // Lose the successful result. A replacement publisher only has the
    // persisted plan and bytes; no receipt or ETag from the first upload.
    store.upload(&p, &p.owner, 1000, b"first").await.unwrap();
    let recovered = store.upload(&p, &p.owner, 1001, b"first").await.unwrap();
    let mut changed = p.clone();
    changed.sha256 = hex::encode(Sha256::digest(b"other"));
    assert_eq!(
        store.upload(&changed, &changed.owner, 1001, b"other").await,
        Err(Error::Conflict)
    );
    for mutate in [0, 1, 2] {
        let mut changed = p.clone();
        match mutate {
            0 => changed.owner.boot_id = "different-boot".into(),
            1 => changed.expires_unix_ms += 1,
            _ => {
                changed.seen += 1;
                changed.truncated = true;
            }
        }
        assert_eq!(
            store.upload(&changed, &changed.owner, 1001, b"first").await,
            Err(Error::Conflict)
        );
    }
    assert_eq!(
        store
            .read(&recovered, &p.owner, 1001, 0, 10)
            .await
            .unwrap()
            .bytes,
        b"first"
    );
}

#[tokio::test]
async fn concurrent_identical_writers_and_separate_attempts() {
    let _test = TESTS.lock().await;
    let store = memory();
    let p = plan(b"once");
    let (a, b) = tokio::join!(
        store.upload(&p, &p.owner, 1000, b"once"),
        store.upload(&p, &p.owner, 1000, b"once")
    );
    assert_eq!(a.unwrap(), b.unwrap());
    let r = store.upload(&p, &p.owner, 1000, b"once").await.unwrap();
    let mut newer = p.clone();
    newer.upload_attempt = OperationId::generate();
    newer.sha256 = hex::encode(Sha256::digest(b"next"));
    store
        .upload(&newer, &newer.owner, 1000, b"next")
        .await
        .unwrap();
    assert_ne!(newer.object_key(), p.object_key());
    // A later attempt cannot alter bytes selected by an earlier reference.
    assert_eq!(
        store.read(&r, &p.owner, 1000, 0, 10).await.unwrap().bytes,
        b"once"
    );
}

#[tokio::test]
async fn pinned_owner_retention_missing_and_corrupt_are_distinct() {
    let _test = TESTS.lock().await;
    let store = memory();
    let p = plan(b"private");
    let r = store.upload(&p, &p.owner, 1000, b"private").await.unwrap();
    for field in 0..8 {
        let mut wrong = p.owner.clone();
        match field {
            0 => wrong.project_id = ProjectId::generate(),
            1 => wrong.sandbox_id = SandboxId::generate(),
            2 => wrong.operation_id = OperationId::generate(),
            3 => wrong.allocation_id = AllocationId::generate(),
            4 => wrong.generation += 1,
            5 => wrong.host_id = HostId::generate(),
            6 => wrong.host_epoch += 1,
            _ => wrong.boot_id = "rebooted".into(),
        }
        assert!(matches!(
            store.read(&r, &wrong, 1000, 0, 10).await,
            Err(Error::OwnerMismatch)
        ));
        assert_eq!(
            store.upload(&p, &wrong, 1000, b"private").await,
            Err(Error::OwnerMismatch)
        );
    }
    assert!(matches!(
        store.read(&r, &p.owner, 2000, 0, 10).await,
        Err(Error::Expired)
    ));
    assert_eq!(
        store.upload(&p, &p.owner, 2000, b"private").await,
        Err(Error::Expired)
    );
    assert!(matches!(
        store.read(&r, &p.owner, 999, 0, 10).await,
        Err(Error::InvalidMetadata)
    ));
    let path = Path::from(p.object_key().unwrap());
    // Simulate out-of-band replacement by another principal. Production
    // wrapper exposes neither overwrite nor delete.
    store
        .inner
        .put_opts(&path, b"corrupt".to_vec().into(), PutOptions::default())
        .await
        .unwrap();
    assert!(matches!(
        store.read(&r, &p.owner, 1000, 0, 10).await,
        Err(Error::Corrupt)
    ));
    store.inner.delete(&path).await.unwrap();
    assert!(matches!(
        store.read(&r, &p.owner, 1000, 0, 10).await,
        Err(Error::Missing)
    ));
}

#[tokio::test]
async fn matching_metadata_cannot_hide_content_corruption() {
    let _test = TESTS.lock().await;
    let store = memory();
    let p = plan(b"private");
    let path = Path::from(p.object_key().unwrap());
    let put = store
        .inner
        .put_opts(
            &path,
            b"corrupt".to_vec().into(),
            PutOptions {
                attributes: Attributes::from_iter([(metadata_key(), p.metadata_digest().unwrap())]),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let r = OutputRef {
        plan: p.clone(),
        etag: put.e_tag.unwrap(),
        object_version: put.version,
    };
    // Even with the current ETag and matching metadata, no prefix is served.
    assert!(matches!(
        store.read(&r, &p.owner, 1000, 0, 1).await,
        Err(Error::Corrupt)
    ));
}

#[tokio::test]
async fn partial_oversized_and_interrupted_bodies_never_return_a_prefix() {
    let _test = TESTS.lock().await;
    let store = memory();
    let p = plan(b"complete");
    let r = store.upload(&p, &p.owner, 1000, b"complete").await.unwrap();
    let path = Path::from(p.object_key().unwrap());
    for body in [b"com".to_vec(), b"completeEXTRA".to_vec()] {
        let mut response = store
            .inner
            .get_opts(&path, GetOptions::default())
            .await
            .unwrap();
        // Keep the valid declared length, ETag and metadata; vary the actual
        // body to exercise transport truncation and dishonest Content-Length.
        response.payload = object_store::GetResultPayload::Stream(Box::pin(
            futures_util::stream::iter(vec![Ok(body.into())]),
        ));
        assert_eq!(
            ArtifactStore::verify_response(&p, Some(&r), response).await,
            Err(Error::Corrupt)
        );
    }
    let mut response = store
        .inner
        .get_opts(&path, GetOptions::default())
        .await
        .unwrap();
    response.payload =
        object_store::GetResultPayload::Stream(Box::pin(futures_util::stream::iter(vec![
            Ok(b"com".to_vec().into()),
            Err(object_store::Error::Generic {
                store: "test",
                source: "private provider error must not escape".into(),
            }),
        ])));
    assert_eq!(
        ArtifactStore::verify_response(&p, Some(&r), response).await,
        Err(Error::Unavailable)
    );
}

#[tokio::test]
async fn transfer_capacity_is_bounded_and_released() {
    let _test = TESTS.lock().await;
    let store = memory();
    let p = plan(b"x");
    let permits = TRANSFERS.try_acquire_many(4).unwrap();
    assert_eq!(
        store.upload(&p, &p.owner, 1000, b"x").await,
        Err(Error::Busy)
    );
    drop(permits);
    assert!(store.upload(&p, &p.owner, 1000, b"x").await.is_ok());
}

#[tokio::test]
async fn max_size_combined_cap_and_malformed_references() {
    let _test = TESTS.lock().await;
    let store = memory();
    let bytes = vec![0xfe; MAX_OUTPUT as usize];
    let p = plan(&bytes);
    let r = store.upload(&p, &p.owner, 1000, &bytes).await.unwrap();
    assert_eq!(
        store
            .read(&r, &p.owner, 1000, p.size - 1, 1)
            .await
            .unwrap()
            .bytes,
        [0xfe]
    );
    let mut stderr = p.clone();
    stderr.name = OutputName::Stderr;
    stderr.size = 0;
    stderr.seen = 0;
    stderr.sha256 = hex::encode(Sha256::digest(b""));
    let e = store.upload(&stderr, &p.owner, 1000, b"").await.unwrap();
    let mut refs = OutputRefs {
        stdout: r.clone(),
        stderr: e,
    };
    assert!(refs.validate(&p.owner, MAX_OUTPUT).is_ok());
    assert!(refs.validate(&p.owner, MAX_OUTPUT - 1).is_err());
    refs.stderr.plan.size = 1;
    refs.stderr.plan.seen = 1;
    assert!(refs.validate(&p.owner, MAX_OUTPUT).is_err());
    refs.stderr.plan.size = 0;
    refs.stderr.plan.seen = 0;
    refs.stderr.plan.upload_attempt = OperationId::generate();
    assert!(refs.validate(&p.owner, MAX_OUTPUT).is_err());
    let mut invalid = p.clone();
    invalid.size = MAX_OUTPUT + 1;
    invalid.seen = invalid.size;
    assert_eq!(
        store.upload(&invalid, &p.owner, 1000, b"").await,
        Err(Error::InvalidMetadata)
    );
    for field in 0..8 {
        let mut invalid = r.clone();
        match field {
            0 => invalid.plan.version += 1,
            1 => invalid.plan.sha256 = "F".repeat(64),
            2 => invalid.plan.owner.boot_id = "x".repeat(65),
            3 => invalid.plan.owner.generation = 0,
            4 => invalid.plan.seen -= 1,
            5 => invalid.plan.delete_after_unix_ms = 1999,
            6 => invalid.etag = "\r\nheader".into(),
            _ => invalid.object_version = Some(String::new()),
        }
        assert!(matches!(
            store.read(&invalid, &p.owner, 1000, 0, 1).await,
            Err(Error::InvalidMetadata)
        ));
    }
    let mut json = serde_json::to_value(&r).unwrap();
    json["bucket"] = "attacker".into();
    assert!(serde_json::from_value::<OutputRef>(json).is_err());
}

/// CI runs this explicitly against a pinned, ephemeral MinIO. Ordinary test
/// runs do not silently substitute an in-memory backend for this evidence.
#[tokio::test]
#[ignore = "requires private test bucket and HUDSON_TEST_S3_* environment"]
async fn output_minio_conditional_writes_and_integrity() {
    let _test = TESTS.lock().await;
    let env = |key| std::env::var(key).expect("required test S3 configuration");
    let store = S3Config {
        endpoint: env("HUDSON_TEST_S3_ENDPOINT"),
        region: "us-east-1".into(),
        bucket: env("HUDSON_TEST_S3_BUCKET"),
        access_key: env("HUDSON_TEST_S3_ACCESS_KEY"),
        secret_key: env("HUDSON_TEST_S3_SECRET_KEY"),
        session_token: None,
        allow_loopback_http: true,
    }
    .build()
    .unwrap();
    let p = plan(b"\x00\xffbinary\n");
    let (a, b) = tokio::join!(
        store.upload(&p, &p.owner, 1000, b"\x00\xffbinary\n"),
        store.upload(&p, &p.owner, 1000, b"\x00\xffbinary\n")
    );
    let r = a.unwrap();
    assert_eq!(r, b.unwrap());
    assert_eq!(
        store
            .read(&r, &p.owner, 1500, 0, MAX_CHUNK)
            .await
            .unwrap()
            .bytes,
        b"\x00\xffbinary\n"
    );
    let mut other = p.clone();
    other.sha256 = hex::encode(Sha256::digest(b"badbytes\n"));
    assert_eq!(
        store
            .upload(&other, &other.owner, 1500, b"badbytes\n")
            .await,
        Err(Error::Conflict)
    );
    let mut empty = p.clone();
    empty.name = OutputName::Stderr;
    empty.size = 0;
    empty.seen = 0;
    empty.sha256 = hex::encode(Sha256::digest(b""));
    let er = store.upload(&empty, &p.owner, 1500, b"").await.unwrap();
    assert!(store.read(&er, &p.owner, 1500, 0, 1).await.unwrap().eof);
    // Replace only this test's random key to confirm If-Match and digest checks.
    let path = Path::from(p.object_key().unwrap());
    store
        .inner
        .put_opts(&path, b"tampered\n".to_vec().into(), PutOptions::default())
        .await
        .unwrap();
    assert!(matches!(
        store.read(&r, &p.owner, 1500, 0, 1).await,
        Err(Error::Corrupt)
    ));
    store.inner.delete(&path).await.unwrap();
    assert!(matches!(
        store.read(&r, &p.owner, 1500, 0, 1).await,
        Err(Error::Missing)
    ));
    store
        .inner
        .delete(&Path::from(empty.object_key().unwrap()))
        .await
        .unwrap();
}
