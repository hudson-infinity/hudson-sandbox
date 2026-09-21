use sandbox_artifacts::{
    Error,
    sources::{SourceBackend, SourceBytes},
};
use sandbox_protocol::file_sources::{SourceOwner, SourcePlan, SourceRef};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
};
use tokio::sync::{Mutex, Notify, Semaphore};
#[derive(Debug)]
pub(crate) struct Sources {
    pub data: Mutex<HashMap<String, (SourceRef, Vec<u8>)>>,
    pub lose_upload: AtomicBool,
    pub fail_upload: AtomicBool,
    pub pause_read: AtomicBool,
    pub pause_upload: AtomicBool,
    pub started: Notify,
    pub resume: Semaphore,
    pub reads: AtomicUsize,
}
impl Default for Sources {
    fn default() -> Self {
        Self {
            data: Mutex::new(HashMap::new()),
            lose_upload: AtomicBool::new(false),
            fail_upload: AtomicBool::new(false),
            pause_read: AtomicBool::new(false),
            pause_upload: AtomicBool::new(false),
            started: Notify::new(),
            resume: Semaphore::new(0),
            reads: AtomicUsize::new(0),
        }
    }
}
impl SourceBackend for Sources {
    fn upload<'a>(
        &'a self,
        p: &'a SourcePlan,
        o: &'a SourceOwner,
        _: i64,
        b: &'a [u8],
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<SourceRef, Error>> + Send + 'a>>
    {
        Box::pin(async move {
            assert_eq!(&p.owner, o);
            assert_eq!(p.upload.sha256, Sha256::digest(b).as_slice());
            if self.pause_upload.load(Ordering::SeqCst) {
                self.started.notify_one();
                self.resume.acquire().await.unwrap().forget();
            }
            if self.fail_upload.load(Ordering::SeqCst) {
                return Err(Error::Unavailable);
            }
            let reference = SourceRef {
                plan: p.clone(),
                etag: "test-etag".into(),
                object_version: None,
            };
            let mut data = self.data.lock().await;
            let key = p.object_key().unwrap();
            if let Some((old, bytes)) = data.get(&key) {
                assert_eq!(old, &reference);
                assert_eq!(bytes, b);
            } else {
                data.insert(key, (reference.clone(), b.to_vec()));
            }
            if self.lose_upload.swap(false, Ordering::SeqCst) {
                return Err(Error::Unavailable);
            }
            Ok(reference)
        })
    }
    fn reconcile<'a>(
        &'a self,
        p: &'a SourcePlan,
        o: &'a SourceOwner,
        _: i64,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<SourceRef, Error>> + Send + 'a>>
    {
        Box::pin(async move {
            assert_eq!(&p.owner, o);
            self.data
                .lock()
                .await
                .get(&p.object_key().unwrap())
                .map(|v| v.0.clone())
                .ok_or(Error::Missing)
        })
    }
    fn read<'a>(
        &'a self,
        r: &'a SourceRef,
        o: &'a SourceOwner,
        _: i64,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<SourceBytes, Error>> + Send + 'a>>
    {
        Box::pin(async move {
            self.reads.fetch_add(1, Ordering::SeqCst);
            if self.pause_read.load(Ordering::SeqCst) {
                self.started.notify_one();
                self.resume.acquire().await.unwrap().forget();
            }
            assert_eq!(&r.plan.owner, o);
            let data = self.data.lock().await;
            let (reference, b) = data
                .get(&r.plan.object_key().unwrap())
                .ok_or(Error::Missing)?;
            assert_eq!(reference, r);
            Ok(SourceBytes { bytes: b.clone() })
        })
    }
}
