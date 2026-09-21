use super::*;
use sandbox_protocol::{
    guest as w,
    output::OutputName,
    supervisor::{OutputObservation, OutputRequest},
};
use sandbox_supervisor::archive::{self as a, ArchiveRecord, OutputSource};

#[derive(Debug)]
struct EmptyOutput(OperationId);
#[tonic::async_trait]
impl OutputSource for EmptyOutput {
    async fn read(
        &self,
        name: OutputName,
        offset: u64,
        _limit: u32,
    ) -> Result<w::OutputChunk, Status> {
        if offset != 0 {
            return Err(Status::invalid_argument("simulated output is empty"));
        }
        Ok(w::OutputChunk {
            operation_id: self.0.to_string(),
            stream: match name {
                OutputName::Stdout => w::Stream::Stdout,
                OutputName::Stderr => w::Stream::Stderr,
            } as i32,
            offset: 0,
            data: vec![],
            next_offset: 0,
            at_end: true,
            complete: true,
        })
    }
}
impl FakeHost {
    pub fn with_artifacts(mut self, store: sandbox_artifacts::ArtifactStore) -> Self {
        self.artifacts = Some(store);
        self
    }
    pub async fn lose_next_archive_reply(&self) {
        self.state.lock().await.lose_next_archive_reply = true;
    }
    pub async fn archives_started(&self) -> u64 {
        self.state.lock().await.archives_started
    }
    pub async fn delay_archives(&self, delay: Duration) {
        self.state.lock().await.archive_delay = delay;
    }
    pub(super) async fn output_inner(
        &self,
        request: OutputRequest,
        upload: bool,
    ) -> Result<OutputObservation, Status> {
        let _permit = self
            .archive_workers
            .try_acquire()
            .map_err(|_| Status::resource_exhausted("fake archive workers busy"))?;
        let (ticket, plans) = a::decode(&request, unix_ms()?)?;
        if plans.is_some() != upload {
            return Err(Status::invalid_argument("output request phase mismatch"));
        }
        let store = self
            .artifacts
            .as_ref()
            .ok_or_else(|| Status::failed_precondition("fake output storage not configured"))?;
        if ticket.owner.host_id != self.config.host || ticket.owner.host_epoch != self.config.epoch
        {
            return Err(Status::failed_precondition("wrong fake output host"));
        }
        let (saved, receipt, has_source, delay) = {
            let mut state = self.state.lock().await;
            Self::expire(&mut state);
            if upload {
                state.archives_started += 1;
            }
            let delay = state.archive_delay;
            let ready = state
                .allocations
                .get(&ticket.owner.allocation_id.to_string())
                .is_some_and(|r| r.state == AllocationState::Ready);
            let fence = state
                .fences
                .get_mut(&ticket.owner.allocation_id.to_string())
                .ok_or_else(|| Status::not_found("fake allocation history missing"))?;
            let owner = &fence.owner;
            if owner.project_id != ticket.owner.project_id.to_string()
                || owner.sandbox_id != ticket.owner.sandbox_id.to_string()
                || owner.generation != ticket.owner.generation
            {
                return Err(Status::failed_precondition("fake output owner mismatch"));
            }
            let command = fence
                .commands
                .get(&ticket.owner.operation_id.to_string())
                .ok_or_else(|| Status::not_found("fake command missing"))?;
            let receipt = command
                .receipt
                .as_ref()
                .ok_or_else(|| Status::failed_precondition("fake receipt missing"))?;
            command
                .validate_receipt(ticket.owner.operation_id, receipt)
                .map_err(|_| Status::failed_precondition("invalid fake receipt"))?;
            a::validate_receipt(&ticket, receipt)?;
            let receipt = receipt.clone();
            let old = fence.archives.get(&ticket.owner.operation_id.to_string());
            if let Some(old) = old {
                if old.ticket != ticket
                    || request.publication_revision < old.revision
                    || plans
                        .as_ref()
                        .is_some_and(|p| old.plans.as_ref() != Some(p))
                {
                    return Err(Status::failed_precondition(
                        "fake archive ownership changed",
                    ));
                }
            } else if upload {
                return Err(Status::failed_precondition("prepare first"));
            }
            let saved = ArchiveRecord {
                ticket: ticket.clone(),
                plans: old.and_then(|r| r.plans.clone()),
                revision: request.publication_revision,
            };
            fence
                .archives
                .insert(ticket.owner.operation_id.to_string(), saved.clone());
            (saved, receipt, ready && !fence.stopped, delay)
        };
        if upload {
            tokio::time::sleep(delay).await;
        }
        a::decode(&request, unix_ms()?)?;
        let source = has_source.then_some(EmptyOutput(ticket.owner.operation_id));
        let plans = if let Some(plans) = saved.plans {
            plans
        } else {
            a::collect(
                &ticket,
                &receipt,
                source
                    .as_ref()
                    .ok_or_else(|| Status::unavailable("fake output history missing"))?,
            )
            .await?
            .plans
        };
        let refs = if upload {
            Some(
                tokio::time::timeout(
                    a::ARCHIVE_TIMEOUT,
                    a::upload(
                        store,
                        &ticket,
                        &plans,
                        &receipt,
                        source.as_ref().map(|s| s as &dyn OutputSource),
                        || {
                            let now = unix_ms().unwrap_or(i64::MAX);
                            if now >= request.claim_expires_unix_ms {
                                i64::MAX
                            } else {
                                now
                            }
                        },
                    ),
                )
                .await
                .map_err(|_| Status::unavailable("fake archive timeout"))??,
            )
        } else {
            None
        };
        let mut state = self.state.lock().await;
        let saved = state
            .fences
            .get_mut(&ticket.owner.allocation_id.to_string())
            .and_then(|f| f.archives.get_mut(&ticket.owner.operation_id.to_string()))
            .ok_or_else(|| Status::unavailable("fake archive history missing"))?;
        a::decode(&request, unix_ms()?)?;
        if saved.revision != request.publication_revision
            || saved.ticket != ticket
            || saved.plans.as_ref().is_some_and(|p| p != &plans)
        {
            return Err(Status::failed_precondition("stale fake output publisher"));
        }
        saved.plans = Some(plans.clone());
        if upload && std::mem::take(&mut state.lose_next_archive_reply) {
            return Err(Status::unavailable("injected lost archive acknowledgement"));
        }
        a::observation(
            request,
            self.config.host.to_string(),
            self.config.epoch,
            true,
            unix_ms()?,
            &plans,
            refs.as_ref(),
        )
    }
}
