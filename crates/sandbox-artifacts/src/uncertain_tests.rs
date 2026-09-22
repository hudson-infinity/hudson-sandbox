#![allow(clippy::unwrap_used)]
use super::*;
use futures_util::stream::BoxStream;
use object_store::{
    CopyOptions, GetResult, ListResult, MultipartUpload, ObjectMeta, PutMultipartOptions,
    PutPayload, PutResult,
};
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Debug)]
struct LostAck {
    inner: object_store::memory::InMemory,
    commit: bool,
    unreadable: bool,
    puts: AtomicUsize,
    gets: AtomicUsize,
}
impl fmt::Display for LostAck {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("fault fixture")
    }
}
fn lost() -> object_store::Error {
    object_store::Error::Generic {
        store: "fixture",
        source: "lost response".into(),
    }
}
#[async_trait::async_trait]
impl ObjectStore for LostAck {
    async fn put_opts(
        &self,
        path: &Path,
        payload: PutPayload,
        opts: PutOptions,
    ) -> object_store::Result<PutResult> {
        self.puts.fetch_add(1, Ordering::SeqCst);
        assert!(matches!(opts.mode, PutMode::Create));
        if self.commit {
            let _ = self.inner.put_opts(path, payload, opts).await;
        }
        Err(lost())
    }
    async fn get_opts(&self, path: &Path, opts: GetOptions) -> object_store::Result<GetResult> {
        self.gets.fetch_add(1, Ordering::SeqCst);
        if self.unreadable {
            Err(lost())
        } else {
            self.inner.get_opts(path, opts).await
        }
    }
    async fn put_multipart_opts(
        &self,
        path: &Path,
        opts: PutMultipartOptions,
    ) -> object_store::Result<Box<dyn MultipartUpload>> {
        self.inner.put_multipart_opts(path, opts).await
    }
    fn delete_stream(
        &self,
        paths: BoxStream<'static, object_store::Result<Path>>,
    ) -> BoxStream<'static, object_store::Result<Path>> {
        self.inner.delete_stream(paths)
    }
    fn list(&self, prefix: Option<&Path>) -> BoxStream<'static, object_store::Result<ObjectMeta>> {
        self.inner.list(prefix)
    }
    async fn list_with_delimiter(&self, prefix: Option<&Path>) -> object_store::Result<ListResult> {
        self.inner.list_with_delimiter(prefix).await
    }
    async fn copy_opts(
        &self,
        from: &Path,
        to: &Path,
        opts: CopyOptions,
    ) -> object_store::Result<()> {
        self.inner.copy_opts(from, to, opts).await
    }
}
fn fixture(commit: bool, unreadable: bool) -> (ArtifactStore, Arc<LostAck>) {
    let fault = Arc::new(LostAck {
        inner: object_store::memory::InMemory::new(),
        commit,
        unreadable,
        puts: AtomicUsize::new(0),
        gets: AtomicUsize::new(0),
    });
    (
        ArtifactStore {
            inner: fault.clone(),
        },
        fault,
    )
}
#[tokio::test]
async fn uncertain_put_reads_once_without_replaying_and_requires_complete_evidence() {
    let _test = crate::tests::TESTS.lock().await;
    for (commit, unreadable, conflict) in [
        (true, false, false),
        (false, false, false),
        (true, true, false),
        (true, false, true),
    ] {
        let (store, fault) = fixture(commit, unreadable);
        let bytes = b"payload";
        let p = crate::tests::plan(bytes);
        if conflict {
            fault
                .inner
                .put_opts(
                    &Path::from(p.object_key().unwrap()),
                    b"retired".to_vec().into(),
                    PutOptions::default(),
                )
                .await
                .unwrap();
        }
        let result = store.upload(&p, &p.owner, 1000, bytes).await;
        if conflict {
            assert_eq!(result, Err(Error::Conflict));
        } else if commit && !unreadable {
            assert!(result.is_ok(), "{result:?}");
        } else {
            assert_eq!(result, Err(Error::Unavailable));
        }
        assert_eq!(fault.puts.load(Ordering::SeqCst), 1);
        assert_eq!(fault.gets.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn uncertain_source_put_preserves_unknown_absence_and_verifies_existing_bytes() {
    use sandbox_protocol::{
        Id, OperationId,
        file_downloads::ReadScope,
        file_sources::{SourceOwner, SourcePlan},
        files::Upload,
    };
    let _test = crate::tests::TESTS.lock().await;
    for (commit, unreadable, conflict) in [
        (true, false, false),
        (false, false, false),
        (true, true, false),
        (true, false, true),
    ] {
        let (store, fault) = fixture(commit, unreadable);
        let store = crate::sources::SourceStore { store };
        let bytes = b"payload";
        let o = crate::tests::owner();
        let owner = SourceOwner {
            operation_id: o.operation_id,
            scope: ReadScope {
                version: 1,
                project_id: o.project_id,
                sandbox_id: o.sandbox_id,
                allocation_id: o.allocation_id,
                host_id: o.host_id,
                host_epoch: o.host_epoch,
                generation: o.generation,
            },
        };
        let p = SourcePlan {
            version: 1,
            owner,
            upload: Upload {
                operation_id: o.operation_id,
                path: "fixture.txt".into(),
                size: bytes.len() as u64,
                sha256: Sha256::digest(bytes).into(),
                mode: 0o644,
            },
            source_attempt: OperationId::generate(),
            created_unix_ms: 1000,
            write_expires_unix_ms: 1500,
            expires_unix_ms: 2000,
            delete_after_unix_ms: 3000,
        };
        if conflict {
            fault
                .inner
                .put_opts(
                    &Path::from(p.object_key().unwrap()),
                    b"retired".to_vec().into(),
                    PutOptions::default(),
                )
                .await
                .unwrap();
        }
        let result = store.upload(&p, &p.owner, 1000, bytes).await;
        if conflict {
            assert_eq!(result, Err(Error::Conflict));
        } else if commit && !unreadable {
            assert!(result.is_ok(), "{result:?}");
        } else {
            assert_eq!(result, Err(Error::Unavailable));
        }
        assert_eq!(fault.puts.load(Ordering::SeqCst), 1);
        assert_eq!(fault.gets.load(Ordering::SeqCst), 1);
    }
}
