use super::*;
use crate::launch_authority::{AuthorityFile, LaunchGuard};
use sandbox_protocol::{
    allocation_authority::Permit,
    supervisor::{AllocationAuthorityObservation, AllocationAuthorityRequest},
};

pub(super) fn open(
    config: &Config,
    journal: &mut Journal,
) -> anyhow::Result<Option<AuthorityFile>> {
    anyhow::ensure!(
        config.launch_permits_required == journal.launch_authority.is_some(),
        "launch authority mode changed; legacy migration is not supported"
    );
    let Some(retained) = &journal.launch_authority else {
        return Ok(None);
    };
    anyhow::ensure!(
        retained.host == config.host && retained.epoch > 0 && retained.epoch <= config.epoch,
        "invalid launch checkpoint"
    );
    let authority = AuthorityFile::open(
        config.state_root.join("a"),
        config.host,
        retained.epoch,
        retained.registered_through,
    )?;
    let current = authority.retained_checkpoint()?;
    anyhow::ensure!(
        current.epoch <= config.epoch,
        "authority epoch exceeds configured epoch"
    );
    if current.epoch < config.epoch {
        authority.advance_epoch(current.epoch, config.epoch)?;
    }
    journal.launch_authority = Some(authority.checkpoint(config.epoch)?);
    journal::save(config, journal)?;
    Ok(Some(authority))
}
impl Host {
    pub(super) fn allocation_authority_sync(
        &self,
        request: AllocationAuthorityRequest,
    ) -> Result<AllocationAuthorityObservation, Status> {
        if request.host_id != self.inner.config.host.to_string()
            || request.reporting_epoch != self.inner.config.epoch
        {
            return Err(Status::failed_precondition("wrong authority host or epoch"));
        }
        let authority =
            self.inner.authority.as_ref().ok_or_else(|| {
                Status::failed_precondition("launch authority is not provisioned")
            })?;
        // The journal and authority advance together before acknowledgement.
        // A lost acknowledgement is reconciled by an empty inspection request.
        let mut journal = self.journal()?;
        let checkpoint = if request.permits_json.is_empty() {
            authority
                .checkpoint(request.reporting_epoch)
                .map_err(uncertain)?
        } else {
            if request.permits_json.len() > 65536 {
                return Err(Status::invalid_argument("permit batch too large"));
            }
            let permits: Vec<Permit> = serde_json::from_slice(&request.permits_json)
                .map_err(|_| Status::invalid_argument("invalid permit batch"))?;
            authority
                .register(request.reporting_epoch, &permits)
                .map_err(uncertain)?
        };
        let prior = journal
            .launch_authority
            .as_ref()
            .ok_or_else(|| uncertain("missing authority checkpoint"))?;
        if checkpoint.registered_through < prior.registered_through {
            return Err(uncertain("authority rollback"));
        }
        if prior != &checkpoint {
            journal.launch_authority = Some(checkpoint.clone());
            self.save(&mut journal)?;
        }
        Ok(AllocationAuthorityObservation {
            host_id: checkpoint.host.to_string(),
            reporting_epoch: checkpoint.epoch,
            registered_through: checkpoint.registered_through,
        })
    }
    pub(super) fn authorize_create(
        &self,
        owner: &Ownership,
        request: &CreateRequest,
    ) -> Result<Option<LaunchGuard>, Status> {
        if self.inner.authority.is_none() {
            if !request.launch_permit_json.is_empty() {
                return Err(Status::failed_precondition(
                    "permit requires provisioned authority",
                ));
            }
            return Ok(None);
        }
        if request.launch_permit_json.len() > 4096 {
            return Err(Status::invalid_argument("permit too large"));
        }
        let permit: Permit = serde_json::from_slice(&request.launch_permit_json)
            .map_err(|_| Status::invalid_argument("launch permit required"))?;
        if permit.host.to_string() != owner.host_id
            || permit.project.to_string() != owner.project_id
            || permit.sandbox.to_string() != owner.sandbox_id
            || permit.allocation.to_string() != owner.allocation_id
            || permit.create_operation.to_string() != owner.operation_id
            || permit.generation != owner.generation
            || permit.original_epoch != owner.supervisor_epoch
        {
            return Err(Status::failed_precondition(
                "launch permit ownership mismatch",
            ));
        }
        crate::launch_authority::authorize(&self.inner.config.state_root.join("a"), Some(&permit))
            .map(Some)
            .map_err(uncertain)
    }
}
