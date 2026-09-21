//! Independent retirement of expired file source objects. No VM credentials.
use sandbox_artifacts::{Error as ArtifactError, sources::SourceRetirer};
use sandbox_protocol::{
    OperationId,
    file_sources::{SourceOwner, SourcePlan, SourceRef, SourceRetirement},
};
use sandbox_store::{Store, dispatch::DispatchError, uploads::cleanup::SourceCleanupClaim};
use std::{fmt, future::Future, time::Duration};
const QUERY_TIMEOUT: Duration = Duration::from_secs(5);
const PROCESS_TIMEOUT: Duration = Duration::from_secs(60);

pub trait SourceRetirementBackend: Send + Sync {
    fn retire(
        &self,
        plan: &SourcePlan,
        owner: &SourceOwner,
        selected: Option<&SourceRef>,
        now: i64,
    ) -> impl Future<Output = Result<SourceRetirement, ArtifactError>> + Send;
}
impl SourceRetirementBackend for SourceRetirer {
    async fn retire(
        &self,
        plan: &SourcePlan,
        owner: &SourceOwner,
        selected: Option<&SourceRef>,
        now: i64,
    ) -> Result<SourceRetirement, ArtifactError> {
        SourceRetirer::retire(self, plan, owner, selected, now).await
    }
}
pub struct SourceCleaner<R = SourceRetirer> {
    store: Store,
    retirer: R,
}
impl<R> fmt::Debug for SourceCleaner<R> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SourceCleaner").finish_non_exhaustive()
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceCleanupTick {
    Idle,
    Completed(OperationId),
}
#[derive(Debug, thiserror::Error)]
pub enum SourceCleanupError {
    #[error("source cleanup metadata validation or database operation failed")]
    Store(#[from] DispatchError),
    #[error("source storage retirement failed")]
    Storage(#[from] ArtifactError),
    #[error("source cleanup deadline exceeded; reconcile the retained attempt")]
    Timeout,
}
async fn query<T>(
    f: impl Future<Output = Result<T, DispatchError>>,
) -> Result<T, SourceCleanupError> {
    tokio::time::timeout(QUERY_TIMEOUT, f)
        .await
        .map_err(|_| SourceCleanupError::Timeout)?
        .map_err(Into::into)
}
impl<R: SourceRetirementBackend> SourceCleaner<R> {
    pub fn new(store: Store, retirer: R) -> Self {
        Self { store, retirer }
    }
    pub async fn tick(&self) -> Result<SourceCleanupTick, SourceCleanupError> {
        let Some(claim) = query(self.store.claim_file_source_cleanup(120)).await? else {
            return Ok(SourceCleanupTick::Idle);
        };
        let result = tokio::time::timeout(PROCESS_TIMEOUT, self.process(&claim))
            .await
            .unwrap_or(Err(SourceCleanupError::Timeout));
        if result.is_err() {
            let shift = claim.revision.saturating_sub(1).clamp(0, 10) as u32;
            let _ = query(
                self.store
                    .defer_file_source_cleanup(&claim, (5u32 * (1 << shift)).min(3600)),
            )
            .await;
        }
        result
    }
    async fn process(
        &self,
        claim: &SourceCleanupClaim,
    ) -> Result<SourceCleanupTick, SourceCleanupError> {
        let work = query(self.store.prepare_file_source_cleanup(claim)).await?;
        let m = &work.manifest;
        let receipt = self
            .retirer
            .retire(
                &m.plan,
                &m.plan.owner,
                m.selected.as_ref(),
                work.now_unix_ms,
            )
            .await?;
        query(self.store.complete_file_source_cleanup(claim, &receipt)).await?;
        Ok(SourceCleanupTick::Completed(claim.operation_id))
    }
}
