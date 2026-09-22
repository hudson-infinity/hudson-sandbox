//! A stopped allocation is a stronger replay fence, but never an outcome proof.
use super::*;
use sandbox_protocol::{
    history::Domain,
    supervisor::{ReleasedHistoryObservation, ReleasedHistoryRequest},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Retirement {
    version: u32,
    through: OperationId,
    requested_through: OperationId,
    revision: i64,
    reporting_epoch: i64,
}
fn slot(record: &Record, domain: Domain) -> &Option<Retirement> {
    match domain {
        Domain::Commands => &record.released_commands,
        Domain::Files => &record.released_files,
    }
}
fn slot_mut(record: &mut Record, domain: Domain) -> &mut Option<Retirement> {
    match domain {
        Domain::Commands => &mut record.released_commands,
        Domain::Files => &mut record.released_files,
    }
}
pub(super) fn check(record: &Record, domain: Domain, id: OperationId) -> Result<(), Status> {
    if slot(record, domain)
        .as_ref()
        .is_some_and(|r| id <= r.through)
    {
        return Err(Status::failed_precondition(
            "destroyed allocation history retired",
        ));
    }
    Ok(())
}
fn valid_id(id: OperationId) -> anyhow::Result<()> {
    sandbox_protocol::history::advance_operation_id(id, None)?;
    Ok(())
}
fn covered(record: &Record, domain: Domain, through: OperationId) -> anyhow::Result<Vec<String>> {
    let names = match domain {
        Domain::Commands => record.commands.keys().collect::<Vec<_>>(),
        Domain::Files => record.files.keys().collect::<Vec<_>>(),
    };
    let mut found = Vec::new();
    for name in names {
        let id: OperationId = name.parse()?;
        valid_id(id)?;
        if id <= through {
            found.push(name.clone());
        }
    }
    Ok(found)
}
fn eligible(record: &Record, domain: Domain, names: &[String]) -> anyhow::Result<()> {
    // Destruction cannot turn an unknown command or a staging upload into a
    // known outcome. Match any executed record to the original retained boot.
    let boot = record
        .manifest
        .as_ref()
        .map(Manifest::receipt)
        .transpose()?
        .and_then(|r| r.guest_boot_id);
    for name in names {
        let context = match domain {
            Domain::Commands => {
                let c = record
                    .commands
                    .get(name)
                    .ok_or_else(|| anyhow::anyhow!("command missing"))?;
                anyhow::ensure!(c.finished(), "unresolved command");
                if c.not_started {
                    continue;
                }
                c.validate_receipt(
                    name.parse()?,
                    c.receipt
                        .as_ref()
                        .ok_or_else(|| anyhow::anyhow!("receipt missing"))?,
                )?;
                c.context.as_ref()
            }
            Domain::Files => {
                let f = record
                    .files
                    .get(name)
                    .ok_or_else(|| anyhow::anyhow!("file missing"))?;
                f.validate()?;
                if f.not_started {
                    continue;
                }
                anyhow::ensure!(matches!(f.state, 3 | 5), "unresolved upload");
                f.context.as_ref()
            }
        }
        .ok_or_else(|| anyhow::anyhow!("original boot missing"))?;
        anyhow::ensure!(
            context.allocation_id.to_string() == record.owner.allocation_id
                && context.generation == record.owner.generation
                && boot.as_deref() == Some(context.boot_id.as_str()),
            "original boot mismatch"
        );
    }
    Ok(())
}
fn reclaim(
    record: &mut Record,
    domain: Domain,
    through: OperationId,
    revision: i64,
    epoch: i64,
) -> anyhow::Result<OperationId> {
    valid_id(through)?;
    anyhow::ensure!(
        record.stopped && revision > 0 && epoch >= record.owner.supervisor_epoch,
        "invalid destruction retirement"
    );
    let completed = if let Some(old) = slot(record, domain) {
        anyhow::ensure!(
            revision >= old.revision && epoch >= old.reporting_epoch,
            "stale destruction retirement"
        );
        anyhow::ensure!(
            revision != old.revision
                || (through == old.requested_through && epoch == old.reporting_epoch),
            "retirement claim changed"
        );
        through.max(old.through)
    } else {
        through
    };
    let names = covered(record, domain, completed)?;
    eligible(record, domain, &names)?;
    for name in names {
        match domain {
            Domain::Commands => {
                record.commands.remove(&name);
                record.archives.remove(&name);
            }
            Domain::Files => {
                record.files.remove(&name);
            }
        }
        record.revisions.remove(&name);
    }
    *slot_mut(record, domain) = Some(Retirement {
        version: 1,
        through: completed,
        requested_through: through,
        revision,
        reporting_epoch: epoch,
    });
    Ok(completed)
}
pub(super) fn validate_retained(record: &Record, epoch: i64) -> anyhow::Result<()> {
    for domain in [Domain::Commands, Domain::Files] {
        if let Some(r) = slot(record, domain) {
            valid_id(r.through)?;
            valid_id(r.requested_through)?;
            anyhow::ensure!(
                record.stopped
                    && r.version == 1
                    && r.revision > 0
                    && r.requested_through <= r.through
                    && r.reporting_epoch >= record.owner.supervisor_epoch
                    && r.reporting_epoch <= epoch,
                "invalid destruction retirement proof"
            );
            anyhow::ensure!(
                covered(record, domain, r.through)?.is_empty(),
                "retired history still present"
            );
        }
    }
    Ok(())
}
impl Host {
    pub(super) fn retire_released_history_sync(
        &self,
        request: ReleasedHistoryRequest,
    ) -> Result<ReleasedHistoryObservation, Status> {
        let owner = request
            .ownership
            .as_ref()
            .ok_or_else(|| Status::invalid_argument("original ownership required"))?;
        if request.reporting_epoch != self.inner.config.epoch
            || owner.supervisor_epoch <= 0
            || owner.supervisor_epoch > request.reporting_epoch
        {
            return Err(Status::failed_precondition("invalid retirement epochs"));
        }
        let mut validated = self.owner(Some(Ownership {
            host_id: owner.host_id.clone(),
            project_id: owner.project_id.clone(),
            sandbox_id: owner.sandbox_id.clone(),
            allocation_id: owner.allocation_id.clone(),
            operation_id: OperationId::generate().to_string(),
            generation: owner.generation,
            supervisor_epoch: request.reporting_epoch,
            claim_revision: owner.revision,
            claim_expires_unix_ms: owner.claim_expires_unix_ms,
        }))?;
        validated.supervisor_epoch = owner.supervisor_epoch;
        let domain = match request.domain {
            1 => Domain::Commands,
            2 => Domain::Files,
            _ => return Err(Status::invalid_argument("invalid retirement domain")),
        };
        let through = request
            .through
            .parse()
            .map_err(|_| Status::invalid_argument("invalid retirement prefix"))?;
        valid_id(through).map_err(|_| Status::invalid_argument("invalid retirement prefix"))?;
        let gate = self
            .journal()?
            .records
            .get(&owner.allocation_id)
            .ok_or_else(|| Status::failed_precondition("retained ownership required"))?
            .gate
            .clone();
        let _gate = lock(&gate)?;
        deadline(owner.claim_expires_unix_ms)?;
        let record = self
            .journal()?
            .records
            .get(&owner.allocation_id)
            .ok_or_else(|| uncertain("retained ownership missing"))?
            .clone();
        if !same_allocation(&record.owner, &validated) || !record.stopped {
            return Err(Status::failed_precondition("original owner is not fenced"));
        }
        // Even retries re-observe the original guardian/cgroup/filesystem fence.
        // A retained boolean, missing journal or expired lease is insufficient.
        let (state, _) = self.observe_record(&record)?;
        if !matches!(
            state,
            AllocationState::Released | AllocationState::FencedAbsent
        ) {
            return Err(uncertain("allocation cleanup unconfirmed"));
        }
        deadline(owner.claim_expires_unix_ms)?;
        let mut j = self.journal()?;
        let mut next = j
            .records
            .get(&owner.allocation_id)
            .ok_or_else(|| uncertain("retained ownership missing"))?
            .clone();
        let completed = reclaim(
            &mut next,
            domain,
            through,
            owner.revision,
            request.reporting_epoch,
        )
        .map_err(|_| Status::failed_precondition("unresolved history or changed claim"))?;
        let before = serde_json::to_vec(
            j.records
                .get(&owner.allocation_id)
                .ok_or_else(|| uncertain("retained ownership missing"))?,
        )
        .map_err(uncertain)?
        .len();
        let total = serde_json::to_vec(&*j).map_err(uncertain)?.len();
        let after = serde_json::to_vec(&next).map_err(uncertain)?.len();
        if total.saturating_sub(before).saturating_add(after) > journal::MAX_BYTES as usize {
            return Err(Status::resource_exhausted(
                "host journal lacks retirement headroom",
            ));
        }
        deadline(owner.claim_expires_unix_ms)?;
        // One durable replacement drops known records and retains proof plus the
        // stronger allocation tombstone. Save failures poison the journal.
        j.records.insert(owner.allocation_id.clone(), next);
        self.save(&mut j)?;
        Ok(ReleasedHistoryObservation {
            request: Some(request),
            completed_through: completed.to_string(),
            release_state: state as i32,
            simulated: false,
            observed_unix_ms: guardian::wall_ms(),
        })
    }
}

#[cfg(test)]
#[path = "released_history_tests.rs"]
mod tests;
