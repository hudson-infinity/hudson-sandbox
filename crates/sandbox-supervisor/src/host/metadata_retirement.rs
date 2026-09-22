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
    if let Some(saved) = &record.metadata_retirement {
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
        let session = Session::open(
            &root,
            &config.cgroup_parent,
            retained.epoch,
            checkpoint.registered_through,
            &request.intent,
            record.manifest.as_ref(),
            Some(&saved.plan),
        )?;
        anyhow::ensure!(
            !saved.removed || session.is_removed(),
            "completed deletion directory reappeared"
        );
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
        let (mut record, checkpoint) = {
            let j = self.journal()?;
            (
                j.records
                    .get(&id)
                    .cloned()
                    .ok_or_else(|| uncertain("retirement record missing"))?,
                j.launch_authority
                    .clone()
                    .ok_or_else(|| uncertain("authority checkpoint missing"))?,
            )
        };
        let gate = record.gate.clone();
        let _gate = gate
            .try_lock()
            .map_err(|_| Status::unavailable("allocation deletion busy"))?;
        // Re-read after taking the allocation gate: a newer claim can win
        // between the initial fence and this deletion attempt.
        record = self
            .journal()?
            .records
            .get(&id)
            .cloned()
            .ok_or_else(|| uncertain("retirement record missing"))?;
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
        let mut session = Session::open(
            &self.inner.config.state_root.join("a"),
            &self.inner.config.cgroup_parent,
            self.inner.config.epoch,
            checkpoint.registered_through,
            &request.intent,
            record.manifest.as_ref(),
            record.metadata_retirement.as_ref().map(|s| &s.plan),
        )
        .map_err(uncertain)?;
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
