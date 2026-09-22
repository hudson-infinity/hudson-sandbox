//! Database completion precedes this RPC. Never translate retired denial into
//! physical cleanup or a new generic host receipt.
use super::*;
use crate::launch_authority::{AuthorityFile, RetirementPhase};
use sandbox_protocol::{
    allocation_retirement::ForgetRequest,
    supervisor::{AllocationForgetObservation, AllocationForgetRequest, AllocationForgetState},
};
impl Host {
    pub(super) fn forget_allocation_sync(
        &self,
        wire: AllocationForgetRequest,
    ) -> Result<AllocationForgetObservation, Status> {
        let request = ForgetRequest::decode(&wire.request_json)
            .map_err(|_| Status::invalid_argument("invalid forgetting request"))?;
        let config = &self.inner.config;
        request
            .validate(config.epoch, guardian::wall_ms())
            .map_err(|_| Status::failed_precondition("stale forgetting claim"))?;
        if !config.launch_permits_required || request.claim.intent.permit.host != config.host {
            return Err(Status::failed_precondition(
                "physical registered host required",
            ));
        }
        let intent = &request.claim.intent;
        let id = intent.permit.allocation.to_string();
        let mut j = self.journal()?;
        let checkpoint = j
            .launch_authority
            .as_ref()
            .ok_or_else(|| uncertain("authority checkpoint missing"))?;
        let record = j.records.get(&id).cloned();
        let _gate = record
            .as_ref()
            .map(|r| {
                r.gate
                    .try_lock()
                    .map_err(|_| Status::unavailable("allocation forgetting busy"))
            })
            .transpose()?;
        let _readers = record
            .as_ref()
            .map(|r| {
                r.readers
                    .clone()
                    .try_write_owned()
                    .map_err(|_| Status::unavailable("allocation readers remain"))
            })
            .transpose()?;
        if lock(&self.inner.downloads)?.pending_allocation(intent.permit.allocation, Instant::now())
        {
            return Err(Status::unavailable("allocation capture tickets remain"));
        }
        let root = config.state_root.join("a");
        let authority = AuthorityFile::open(
            root.clone(),
            config.host,
            config.epoch,
            checkpoint.registered_through,
        )
        .map_err(uncertain)?;
        // Bind the latest journal frontier while obtaining the exclusive root
        // lock; retain it through every durable write below.
        let mut guard = authority
            .retirement_guard(config.epoch, intent)
            .map_err(uncertain)?;
        deadline(request.claim.expires_unix_ms)?;
        match &record {
            Some(record) => {
                if guard.phase == RetirementPhase::Closed {
                    return Err(uncertain("record survived forgotten authority"));
                }
                metadata_retirement::verify_forgetting(config, record, &request)
                    .map_err(uncertain)?;
                if guard.phase == RetirementPhase::Fenced {
                    guard.complete().map_err(uncertain)?;
                }
                deadline(request.claim.expires_unix_ms)?;
                j.records.remove(&id);
                self.save(&mut j)?;
            }
            None => {
                if guard.phase == RetirementPhase::Fenced {
                    return Err(uncertain("missing metadata completion record"));
                }
                guardian::retirement::verify_absent(&root, &config.cgroup_parent, intent)
                    .map_err(uncertain)?;
            }
        }
        let state = if guard.phase == RetirementPhase::Closed {
            AllocationForgetState::Retired
        } else {
            // Even when already absent after a crash, sync the containing
            // journal directory before discarding the root completion proof.
            fs::File::open(&config.state_root)
                .and_then(|f| f.sync_all())
                .map_err(uncertain)?;
            deadline(request.claim.expires_unix_ms)?;
            guard.forget().map_err(uncertain)?;
            AllocationForgetState::Forgotten
        };
        deadline(request.claim.expires_unix_ms)?;
        Ok(AllocationForgetObservation {
            request: Some(wire),
            state: state as i32,
            observed_unix_ms: guardian::wall_ms(),
        })
    }
}
