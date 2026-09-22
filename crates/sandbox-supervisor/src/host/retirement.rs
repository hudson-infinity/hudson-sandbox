//! Persist exact scope before root fencing. No physical cleanup or deletion here.
use super::*;
use sandbox_protocol::{
    allocation_retirement::{DomainClosure, Request as RetirementRequest},
    history::Domain,
    supervisor::{AllocationFenceObservation, AllocationFenceRequest},
};

pub(super) fn check(record: &Record) -> Result<(), Status> {
    if record.retirement.is_some() {
        return Err(Status::failed_precondition(
            "allocation retirement in progress; no release proof",
        ));
    }
    Ok(())
}
fn scope(record: &Record, request: &RetirementRequest) -> anyhow::Result<()> {
    let p = &request.intent.permit;
    anyhow::ensure!(
        record.owner.host_id == p.host.to_string()
            && record.owner.project_id == p.project.to_string()
            && record.owner.sandbox_id == p.sandbox.to_string()
            && record.owner.allocation_id == p.allocation.to_string()
            && record.owner.generation == p.generation
            && record.owner.supervisor_epoch == p.original_epoch,
        "retirement owner changed"
    );
    if let Some(manifest) = &record.manifest {
        anyhow::ensure!(
            manifest.launch_permit.as_ref() == Some(p)
                && manifest.start.owner.create_operation == p.create_operation,
            "retirement create owner changed"
        );
    }
    anyhow::ensure!(
        record.stopped
            || (record.create.is_none() && record.manifest.is_none() && !record.dispatched),
        "allocation is not stopped"
    );
    anyhow::ensure!(
        record.commands.is_empty() && record.files.is_empty() && record.archives.is_empty(),
        "retirement consumers remain"
    );
    for (domain, expected) in [
        (Domain::Commands, &request.intent.commands),
        (Domain::Files, &request.intent.files),
    ] {
        let actual = history::closure(record, domain)?;
        anyhow::ensure!(
            match expected {
                DomainClosure::Empty {} => actual.is_none(),
                DomainClosure::Retired { through } => actual == Some(*through),
            },
            "retirement domain scope changed"
        );
    }
    Ok(())
}
pub(super) fn validate_retained(record: &Record, epoch: i64) -> anyhow::Result<()> {
    if let Some(request) = &record.retirement {
        request.intent.validate()?;
        anyhow::ensure!(
            !request.intent.simulated
                && request.revision > 0
                && request.expires_unix_ms > 0
                && request.reporting_epoch >= request.intent.permit.original_epoch
                && request.reporting_epoch <= epoch
                && record.stopped,
            "invalid retained retirement request"
        );
        scope(record, request)?;
    }
    Ok(())
}
fn empty(request: &RetirementRequest) -> Record {
    let p = &request.intent.permit;
    Record {
        owner: Ownership {
            host_id: p.host.to_string(),
            project_id: p.project.to_string(),
            sandbox_id: p.sandbox.to_string(),
            allocation_id: p.allocation.to_string(),
            operation_id: p.create_operation.to_string(),
            generation: p.generation,
            supervisor_epoch: p.original_epoch,
            claim_revision: request.revision,
            claim_expires_unix_ms: request.expires_unix_ms,
        },
        retirement: None,
        revisions: BTreeMap::new(),
        create: None,
        manifest: None,
        dispatched: false,
        stopped: true,
        released: false,
        commands: BTreeMap::new(),
        files: BTreeMap::new(),
        archives: BTreeMap::new(),
        command_history: None,
        file_history: None,
        released_commands: None,
        released_files: None,
        lease_revision: 0,
        lease_request: None,
        gate: Arc::new(Mutex::new(())),
        file_io: journal::file_io(),
        readers: Arc::default(),
    }
}
impl Host {
    pub(super) fn fence_allocation_sync(
        &self,
        wire: AllocationFenceRequest,
    ) -> Result<AllocationFenceObservation, Status> {
        let request = RetirementRequest::decode(&wire.request_json)
            .map_err(|_| Status::invalid_argument("invalid retirement request"))?;
        let config = &self.inner.config;
        if self.inner.authority.is_none()
            || request.intent.simulated
            || request.intent.permit.host != config.host
        {
            return Err(Status::failed_precondition(
                "registered physical retirement owner required",
            ));
        }
        deadline(request.expires_unix_ms)?;
        request
            .validate(&request.intent, config.epoch, 1, guardian::wall_ms())
            .map_err(|_| Status::failed_precondition("invalid retirement claim"))?;
        let mut journal = self.journal()?;
        let checkpoint = journal
            .launch_authority
            .clone()
            .ok_or_else(|| uncertain("missing authority checkpoint"))?;
        let root = config.state_root.join("a");
        let authority_guard = crate::launch_authority::authorize_retirement(
            &root,
            config.host,
            config.epoch,
            checkpoint.registered_through,
            &request.intent,
        )
        .map_err(|_| Status::failed_precondition("retirement authority rejected"))?;
        let id = request.intent.permit.allocation.to_string();
        let mut record = match journal.records.get(&id) {
            Some(record) => record.clone(),
            None => {
                if journal.records.len() >= journal::MAX_RECORDS {
                    return Err(Status::resource_exhausted("host receipt capacity full"));
                }
                empty(&request)
            }
        };
        let gate = record.gate.clone();
        // Never wait for an allocation gate while holding the journal mutex.
        let _gate = gate
            .try_lock()
            .map_err(|_| Status::unavailable("allocation retirement busy"))?;
        scope(&record, &request)
            .map_err(|_| Status::failed_precondition("retirement scope is not closed"))?;
        if let Some(old) = &record.retirement {
            request
                .validate(&old.intent, config.epoch, old.revision, guardian::wall_ms())
                .map_err(|_| Status::failed_precondition("retirement scope or claim changed"))?;
            if request.revision == old.revision && &request != old {
                return Err(Status::failed_precondition(
                    "retirement claim changed without revision",
                ));
            }
        }
        record.retirement = Some(request.clone());
        record.stopped = true;
        let readers = record.readers.clone();
        journal.records.insert(id, record);
        self.save(&mut journal)?;
        drop(journal);
        drop(authority_guard);
        // A failed fence retains the intent and denies generic host mutations.
        // It can still leave the previous root grant active: no acknowledgement
        // or cleanup inference is returned until durable fencing succeeds.
        let authority = crate::launch_authority::AuthorityFile::open(
            root,
            config.host,
            config.epoch,
            checkpoint.registered_through,
        )
        .map_err(uncertain)?;
        authority
            .fence(
                config.epoch,
                &request.intent.permit,
                request.intent.retirement,
            )
            .map_err(uncertain)?;
        // The persisted stop closes admission. Never wait while holding the journal.
        let _readers = readers
            .try_write_owned()
            .map_err(|_| Status::unavailable("allocation readers still active"))?;
        if lock(&self.inner.downloads)?
            .pending_allocation(request.intent.permit.allocation, Instant::now())
        {
            return Err(Status::unavailable(
                "allocation capture tickets still active",
            ));
        }
        deadline(request.expires_unix_ms)?;
        Ok(AllocationFenceObservation {
            request: Some(wire),
            observed_unix_ms: guardian::wall_ms(),
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use sandbox_protocol::{
        allocation_authority::Permit, allocation_retirement::Intent, guest_model::Context,
        history::Barrier,
    };
    fn request() -> RetirementRequest {
        RetirementRequest {
            intent: Intent {
                version: 1,
                retirement: OperationId::generate(),
                permit: Permit {
                    host: HostId::generate(),
                    project: ProjectId::generate(),
                    sandbox: SandboxId::generate(),
                    allocation: AllocationId::generate(),
                    create_operation: OperationId::generate(),
                    generation: 1,
                    original_epoch: 1,
                    serial: 1,
                },
                commands: DomainClosure::Empty {},
                files: DomainClosure::Empty {},
                release_evidence_sha256: "ab".repeat(32),
                simulated: false,
            },
            reporting_epoch: 1,
            revision: 1,
            expires_unix_ms: 1000,
        }
    }
    #[tokio::test]
    async fn reader_pins_survive_record_replacement_and_drain_only_after_worker_exit() {
        let request = request();
        let mut record = empty(&request);
        record.stopped = false;
        let reader = readers::pin(&record).unwrap();
        let (ready, started) = tokio::sync::oneshot::channel();
        let (finish, done) = tokio::sync::oneshot::channel();
        let worker = tokio::spawn(async move {
            let _reader = reader;
            ready.send(()).unwrap();
            let _ = done.await;
        });
        started.await.unwrap();
        // Journal updates replace cloned records; all clones must share the pins.
        let mut frozen = record.clone();
        frozen.retirement = Some(request);
        frozen.stopped = true;
        assert!(readers::pin(&frozen).is_err());
        assert!(frozen.readers.clone().try_write_owned().is_err());
        finish.send(()).unwrap();
        worker.await.unwrap();
        let drained = frozen.readers.clone().try_write_owned().unwrap();
        assert!(readers::pin(&record).is_err());
        drop(drained);
        assert!(readers::pin(&frozen).is_err());
        // Cancellation must also release the host pin; it is not proof of guest cleanup.
        let pin = readers::pin(&record).unwrap();
        let worker = tokio::spawn(async move {
            let _pin = pin;
            std::future::pending::<()>().await;
        });
        assert!(record.readers.clone().try_write_owned().is_err());
        worker.abort();
        assert!(worker.await.unwrap_err().is_cancelled());
        assert!(record.readers.clone().try_write_owned().is_ok());
    }
    #[test]
    fn pending_history_or_changed_domain_cannot_be_frozen_as_empty() {
        let mut request = request();
        let mut record = empty(&request);
        let through = OperationId::generate();
        record.command_history = Some(history::Retirement {
            barrier: Barrier {
                version: 1,
                context: Context {
                    allocation_id: request.intent.permit.allocation,
                    generation: 1,
                    boot_id: "boot".into(),
                },
                domain: Domain::Commands,
                through,
            },
            revision: 1,
            requested_through: through,
            completed: false,
        });
        assert!(scope(&record, &request).is_err());
        request.intent.commands = DomainClosure::Retired { through };
        assert!(scope(&record, &request).is_err());
        record.command_history.as_mut().unwrap().completed = true;
        scope(&record, &request).unwrap();
        request.intent.commands = DomainClosure::Empty {};
        assert!(scope(&record, &request).is_err());
    }
    #[test]
    fn retained_fence_rejects_changed_owner_epoch_and_simulation() {
        let request = request();
        let mut record = empty(&request);
        record.retirement = Some(request.clone());
        validate_retained(&record, 2).unwrap();
        assert!(check(&record).is_err());
        record.retirement.as_mut().unwrap().intent.simulated = true;
        assert!(validate_retained(&record, 2).is_err());
        record.retirement = Some(request);
        record.owner.generation += 1;
        assert!(validate_retained(&record, 2).is_err());
        record.owner.generation = 1;
        assert!(validate_retained(&record, 0).is_err());
    }
}
