//! Retain independently verified cleanup before the first owned-file unlink.
use super::*;
use crate::guardian::retirement::{Plan, Session};
use sandbox_protocol::{
    allocation_retirement::Request as RetirementRequest,
    supervisor::{
        AllocationFenceRequest, AllocationMetadataObservation, AllocationMetadataRequest,
    },
};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Saved {
    plan: Plan,
    removed: bool,
}

pub(super) fn receipt(record: &Record) -> anyhow::Result<Receipt> {
    let manifest = record
        .manifest
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("guardian manifest missing"))?;
    if let Some(saved) = &record.metadata_retirement {
        let receipt = saved
            .plan
            .receipt()
            .ok_or_else(|| anyhow::anyhow!("retained cleanup receipt missing"))?;
        manifest.validate_receipt(receipt)?;
        Ok(receipt.clone())
    } else {
        manifest.receipt()
    }
}

pub(super) fn validate_retained(
    config: &Config,
    checkpoint: Option<&crate::launch_authority::Checkpoint>,
    record: &Record,
) -> anyhow::Result<()> {
    if record.retirement.is_some() || record.metadata_retirement.is_some() {
        let request = record
            .retirement
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("deletion lacks retirement intent"))?;
        let checkpoint =
            checkpoint.ok_or_else(|| anyhow::anyhow!("deletion lacks authority checkpoint"))?;
        let root = config.state_root.join("a");
        let authority = crate::launch_authority::AuthorityFile::open(
            root.clone(),
            config.host,
            request.reporting_epoch,
            checkpoint.registered_through,
        )?;
        let retained = authority.retained_checkpoint()?;
        // Journal recovery precedes this process's epoch advance. Never accept a
        // root from a future process or silently lower either retained frontier.
        anyhow::ensure!(
            retained.epoch <= config.epoch && config.launch_permits_required,
            "deletion authority epoch or mode mismatch"
        );
        let Some(saved) = &record.metadata_retirement else {
            // A persisted preparation can precede a failed root fence, but
            // cannot adopt Complete/Closed without its removed metadata plan.
            crate::launch_authority::authorize_retirement(
                &root,
                config.host,
                retained.epoch,
                checkpoint.registered_through,
                &request.intent,
            )?;
            return Ok(());
        };
        if saved.removed {
            let guard = authority.retirement_guard(retained.epoch, &request.intent)?;
            anyhow::ensure!(
                guard.phase != crate::launch_authority::RetirementPhase::Closed,
                "host record survived root forgetting"
            );
            saved.plan.verify_removed(
                &root,
                &config.cgroup_parent,
                &request.intent,
                record.manifest.as_ref(),
            )?;
            return Ok(());
        }
        let _session = Session::open(
            &root,
            &config.cgroup_parent,
            retained.epoch,
            checkpoint.registered_through,
            &request.intent,
            record.manifest.as_ref(),
            Some(&saved.plan),
        )?;
    }
    Ok(())
}
impl Host {
    pub(super) fn retire_allocation_metadata_sync(
        &self,
        wire: AllocationMetadataRequest,
    ) -> Result<AllocationMetadataObservation, Status> {
        // Reconcile exact scope, current claim, root fencing and host readers.
        self.fence_allocation_sync(AllocationFenceRequest {
            request_json: wire.request_json.clone(),
        })?;
        let request = RetirementRequest::decode(&wire.request_json).map_err(uncertain)?;
        let id = request.intent.permit.allocation.to_string();
        let gate = self
            .journal()?
            .records
            .get(&id)
            .ok_or_else(|| uncertain("retirement record missing"))?
            .gate
            .clone();
        let _gate = gate
            .try_lock()
            .map_err(|_| Status::unavailable("allocation deletion busy"))?;
        // Re-read after taking the allocation gate: a newer claim can win
        // between the initial fence and this deletion attempt.
        let journal = self.journal()?;
        let mut record = journal
            .records
            .get(&id)
            .cloned()
            .ok_or_else(|| uncertain("retirement record missing"))?;
        let checkpoint = journal
            .launch_authority
            .as_ref()
            .ok_or_else(|| uncertain("authority checkpoint missing"))?;
        if record.retirement.as_ref() != Some(&request) {
            return Err(Status::failed_precondition("retirement claim superseded"));
        }
        let _readers = record
            .readers
            .clone()
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
        // Release is durable before allocation retirement is eligible, but a
        // guardian can still hold its lifecycle lock while its watchdog exits.
        // Reissue the exact stop before taking that lock; this is idempotent and
        // cannot affect another allocation because the retained manifest has
        // already been bound to the frozen retirement intent above.
        if let Some(manifest) = &record.manifest {
            let _ = guardian::control(manifest, Action::Stop);
        }
        let root = self.inner.config.state_root.join("a");
        let mut session = loop {
            match Session::open(
                &root,
                &self.inner.config.cgroup_parent,
                self.inner.config.epoch,
                checkpoint.registered_through,
                &request.intent,
                record.manifest.as_ref(),
                record.metadata_retirement.as_ref().map(|s| &s.plan),
            ) {
                Ok(session) => break session,
                Err(error) if crate::launch_authority::is_lock_contended(&error) => {
                    // The stop RPC can return before the detached guardian wrapper
                    // releases its shared launch lock and lifecycle lock. Retry
                    // only nonblocking flock contention, within the signed claim.
                    deadline(request.expires_unix_ms)?;
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => {
                    return Err(uncertain(format!(
                        "metadata retirement session open: {error}"
                    )));
                }
            }
        };
        // The latest independent frontier stays locked until the exclusive
        // root gate is held, so concurrent registration cannot stale this check.
        drop(journal);
        if let Some(saved) = &record.metadata_retirement {
            if saved.removed && !session.is_removed() {
                return Err(uncertain("completed deletion reappeared"));
            }
        } else {
            record.metadata_retirement = Some(Saved {
                plan: session.plan.clone(),
                removed: false,
            });
            let mut j = self.journal()?;
            j.records.insert(id.clone(), record.clone());
            // No unlink is permitted until this original cleanup evidence and
            // exact inventory have been durably retained outside the directory.
            self.save(&mut j)?;
        }
        while !session.remove_next().map_err(uncertain)? {}
        record
            .metadata_retirement
            .as_mut()
            .ok_or_else(|| uncertain("deletion intent missing"))?
            .removed = true;
        let mut j = self.journal()?;
        j.records.insert(id, record);
        self.save(&mut j)?;
        deadline(request.expires_unix_ms)?;
        Ok(AllocationMetadataObservation {
            request: Some(wire),
            observed_unix_ms: guardian::wall_ms(),
        })
    }
}

pub(super) fn verify_forgetting(
    config: &Config,
    record: &Record,
    request: &sandbox_protocol::allocation_retirement::ForgetRequest,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        record.retirement.as_ref() == Some(&request.metadata_request),
        "historical metadata claim changed"
    );
    super::retirement::validate_retained(record, config.epoch)?;
    let saved = record
        .metadata_retirement
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("metadata completion missing"))?;
    anyhow::ensure!(saved.removed, "metadata removal is incomplete");
    saved.plan.verify_removed(
        &config.state_root.join("a"),
        &config.cgroup_parent,
        &request.claim.intent,
        record.manifest.as_ref(),
    )
}
