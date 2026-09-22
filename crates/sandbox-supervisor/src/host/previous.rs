//! A new epoch may verify cleanup of retained old ownership, never revive it.
use super::*;
use sandbox_protocol::supervisor::{PreviousAllocationObservation, PreviousAllocationRequest};

impl Host {
    pub(super) fn previous_allocation(
        &self,
        request: PreviousAllocationRequest,
    ) -> Result<PreviousAllocationObservation, Status> {
        let owner = request
            .ownership
            .clone()
            .ok_or_else(|| Status::invalid_argument("original ownership required"))?;
        if request.reporting_epoch != self.inner.config.epoch
            || owner.supervisor_epoch <= 0
            || owner.supervisor_epoch >= request.reporting_epoch
        {
            return Err(Status::failed_precondition("invalid recovery epochs"));
        }
        // Reuse syntax, host identity and deadline validation without weakening
        // the normal mutation entry points' exact-epoch requirement.
        let mut validation = owner.clone();
        validation.supervisor_epoch = request.reporting_epoch;
        self.owner(Some(validation))?;
        let gate = {
            let j = self.journal()?;
            let record = j
                .records
                .get(&owner.allocation_id)
                .ok_or_else(|| Status::failed_precondition("retained ownership unavailable"))?;
            if !same_allocation(&record.owner, &owner) || !record.stopped {
                return Err(Status::failed_precondition("original owner is not fenced"));
            }
            record.gate.clone()
        };
        let _gate = gate
            .try_lock()
            .map_err(|_| Status::unavailable("allocation recovery busy"))?;
        deadline(owner.claim_expires_unix_ms)?;
        let record = {
            let j = self.journal()?;
            let record = j
                .records
                .get(&owner.allocation_id)
                .ok_or_else(|| uncertain("retained ownership unavailable"))?;
            if !same_allocation(&record.owner, &owner) || !record.stopped {
                return Err(uncertain("original owner is not fenced"));
            }
            record.clone()
        };
        super::retirement::check(&record)?;
        // This re-verifies the original guardian/cgroup/filesystem fence even
        // when a prior cleanup receipt already exists. Missing metadata is not
        // upgraded into an absence receipt, and no new journal entry is admitted.
        let (state, receipt) = self.observe_record(&record)?;
        if !matches!(
            state,
            AllocationState::Released | AllocationState::FencedAbsent
        ) {
            return Err(uncertain("original cleanup remains unconfirmed"));
        }
        deadline(owner.claim_expires_unix_ms)?;
        Ok(PreviousAllocationObservation {
            release: Some(self.observation(owner, &record, state, receipt)),
            request: Some(request),
        })
    }
}
