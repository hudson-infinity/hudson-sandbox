//! Controller-approved retirement: persist a host floor before any guest deletion.
use super::*;
use sandbox_protocol::{
    history::{Barrier, Domain},
    supervisor::{HistoryObservation, HistoryRequest},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Retirement {
    pub barrier: Barrier,
    pub revision: i64,
    pub requested_through: OperationId,
    pub completed: bool,
}
fn slot(record: &Record, domain: Domain) -> &Option<Retirement> {
    match domain {
        Domain::Commands => &record.command_history,
        Domain::Files => &record.file_history,
    }
}
fn slot_mut(record: &mut Record, domain: Domain) -> &mut Option<Retirement> {
    match domain {
        Domain::Commands => &mut record.command_history,
        Domain::Files => &mut record.file_history,
    }
}
pub(super) fn check(record: &Record, domain: Domain, id: OperationId) -> Result<(), Status> {
    super::retirement::check(record)?;
    super::released_history::check(record, domain, id)?;
    if slot(record, domain)
        .as_ref()
        .is_some_and(|v| v.barrier.covers(id))
    {
        return Err(Status::failed_precondition("operation history retired"));
    }
    Ok(())
}
fn eligible(record: &Record, barrier: &Barrier) -> anyhow::Result<()> {
    match barrier.domain {
        Domain::Commands => {
            for (id, command) in &record.commands {
                let id = id.parse()?;
                if barrier.covers(id) {
                    anyhow::ensure!(command.finished(), "unresolved command history");
                    if !command.not_started {
                        anyhow::ensure!(
                            command.context.as_ref() == Some(&barrier.context),
                            "command history boot mismatch"
                        );
                        command.validate_receipt(
                            id,
                            command
                                .receipt
                                .as_ref()
                                .ok_or_else(|| anyhow::anyhow!("missing terminal receipt"))?,
                        )?;
                    }
                }
            }
        }
        Domain::Files => {
            for (id, file) in &record.files {
                if barrier.covers(id.parse()?) {
                    file.validate()?;
                    anyhow::ensure!(
                        file.not_started || matches!(file.state, 3 | 5),
                        "unresolved file history"
                    );
                    anyhow::ensure!(
                        file.not_started || file.context.as_ref() == Some(&barrier.context),
                        "file history boot mismatch"
                    );
                }
            }
        }
    }
    Ok(())
}
fn prepare(record: &mut Record, requested: Barrier, revision: i64) -> anyhow::Result<Retirement> {
    anyhow::ensure!(revision > 0, "invalid retirement revision");
    requested.validate(&requested.context, requested.domain)?;
    anyhow::ensure!(
        requested.context.allocation_id.to_string() == record.owner.allocation_id
            && requested.context.generation == record.owner.generation,
        "retirement allocation mismatch"
    );
    if let Some(current) = slot(record, requested.domain) {
        anyhow::ensure!(
            current.barrier.context == requested.context,
            "retirement boot changed"
        );
        anyhow::ensure!(revision >= current.revision, "stale retirement claim");
        anyhow::ensure!(
            revision != current.revision || requested.through == current.requested_through,
            "retirement claim payload changed"
        );
        if requested.through <= current.barrier.through {
            let mut next = current.clone();
            next.revision = revision;
            next.requested_through = requested.through;
            *slot_mut(record, requested.domain) = Some(next.clone());
            return Ok(next);
        }
        anyhow::ensure!(current.completed, "previous retirement incomplete");
    }
    eligible(record, &requested)?;
    let retirement = Retirement {
        requested_through: requested.through,
        barrier: requested,
        revision,
        completed: false,
    };
    *slot_mut(record, retirement.barrier.domain) = Some(retirement.clone());
    Ok(retirement)
}
fn complete(record: &mut Record, retirement: &Retirement) -> anyhow::Result<()> {
    let current = slot(record, retirement.barrier.domain)
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("retirement missing"))?;
    anyhow::ensure!(
        current.barrier == retirement.barrier && current.revision == retirement.revision,
        "retirement ownership changed"
    );
    eligible(record, &retirement.barrier)?;
    let names = match retirement.barrier.domain {
        Domain::Commands => record.commands.keys().collect::<Vec<_>>(),
        Domain::Files => record.files.keys().collect::<Vec<_>>(),
    }
    .into_iter()
    .map(|id| Ok((id.clone(), retirement.barrier.covers(id.parse()?))))
    .collect::<anyhow::Result<Vec<_>>>()?;
    for (id, covered) in names {
        if !covered {
            continue;
        }
        match retirement.barrier.domain {
            Domain::Commands => {
                record.commands.remove(&id);
                record.archives.remove(&id);
            }
            Domain::Files => {
                record.files.remove(&id);
            }
        }
        record.revisions.remove(&id);
    }
    slot_mut(record, retirement.barrier.domain)
        .as_mut()
        .ok_or_else(|| anyhow::anyhow!("retirement missing"))?
        .completed = true;
    Ok(())
}
pub(super) fn validate_retained(record: &Record) -> anyhow::Result<()> {
    for domain in [Domain::Commands, Domain::Files] {
        if let Some(current) = slot(record, domain) {
            current.barrier.validate(&current.barrier.context, domain)?;
            Barrier {
                through: current.requested_through,
                ..current.barrier.clone()
            }
            .validate(&current.barrier.context, domain)?;
            anyhow::ensure!(
                current.requested_through <= current.barrier.through,
                "retirement request exceeds retained floor"
            );
            anyhow::ensure!(
                current.revision > 0
                    && current.barrier.context.allocation_id.to_string()
                        == record.owner.allocation_id
                    && current.barrier.context.generation == record.owner.generation,
                "invalid retained retirement ownership"
            );
            anyhow::ensure!(
                super::metadata_retirement::receipt(record)?
                    .guest_boot_id
                    .as_deref()
                    == Some(current.barrier.context.boot_id.as_str()),
                "retirement boot mismatch"
            );
            eligible(record, &current.barrier)?;
            if current.completed {
                let names = match domain {
                    Domain::Commands => record.commands.keys().collect::<Vec<_>>(),
                    Domain::Files => record.files.keys().collect::<Vec<_>>(),
                };
                for id in names {
                    anyhow::ensure!(
                        !current.barrier.covers(id.parse()?),
                        "completed retirement retains records"
                    );
                }
            }
        }
    }
    Ok(())
}
impl Host {
    fn history_owner(&self, owner: &LeaseOwnership) -> Result<Ownership, Status> {
        self.owner(Some(Ownership {
            host_id: owner.host_id.clone(),
            project_id: owner.project_id.clone(),
            sandbox_id: owner.sandbox_id.clone(),
            allocation_id: owner.allocation_id.clone(),
            operation_id: OperationId::generate().to_string(),
            generation: owner.generation,
            supervisor_epoch: owner.supervisor_epoch,
            claim_revision: owner.revision,
            claim_expires_unix_ms: owner.claim_expires_unix_ms,
        }))
    }
    pub(super) fn history_binding_sync(
        &self,
        request: LeaseInspection,
    ) -> Result<sandbox_protocol::supervisor::HistoryBindingObservation, Status> {
        let owner = request
            .ownership
            .as_ref()
            .ok_or_else(|| Status::invalid_argument("history ownership required"))?;
        let validated = self.history_owner(owner)?;
        let gate = self
            .journal()?
            .records
            .get(&owner.allocation_id)
            .ok_or_else(|| Status::not_found("allocation history missing"))?
            .gate
            .clone();
        let _gate = lock(&gate)?;
        deadline(owner.claim_expires_unix_ms)?;
        let record = self
            .journal()?
            .records
            .get(&owner.allocation_id)
            .ok_or_else(|| uncertain("allocation missing"))?
            .clone();
        if !same_allocation(&record.owner, &validated) || record.stopped || record.released {
            return Err(Status::failed_precondition(
                "original live allocation required",
            ));
        }
        let client = record
            .manifest
            .as_ref()
            .ok_or_else(|| uncertain("guest manifest missing"))?
            .guest_client()
            .map_err(uncertain)?;
        deadline(owner.claim_expires_unix_ms)?;
        Ok(sandbox_protocol::supervisor::HistoryBindingObservation {
            request: Some(request),
            context: Some(client.context().into()),
            simulated: false,
            observed_unix_ms: guardian::wall_ms(),
        })
    }

    pub(super) fn retire_history_sync(
        &self,
        request: HistoryRequest,
    ) -> Result<HistoryObservation, Status> {
        let owner = request
            .ownership
            .as_ref()
            .ok_or_else(|| Status::invalid_argument("retirement ownership required"))?;
        // Reuse strict allocation identity validation, without consuming a customer operation slot.
        let validated = self.history_owner(owner)?;
        let barrier: Barrier = request
            .barrier
            .clone()
            .ok_or_else(|| Status::invalid_argument("history barrier required"))?
            .try_into()
            .map_err(|_| Status::invalid_argument("invalid history barrier"))?;
        // Retirement cannot create an allocation record or invent absence evidence.
        let gate = self
            .journal()?
            .records
            .get(&owner.allocation_id)
            .ok_or_else(|| Status::not_found("allocation history missing"))?
            .gate
            .clone();
        let _gate = lock(&gate)?;
        deadline(owner.claim_expires_unix_ms)?;
        let mut record = self
            .journal()?
            .records
            .get(&owner.allocation_id)
            .ok_or_else(|| uncertain("allocation missing"))?
            .clone();
        if !same_allocation(&record.owner, &validated) {
            return Err(Status::failed_precondition(
                "retirement allocation identity changed",
            ));
        }
        let retirement = prepare(&mut record, barrier, owner.revision)
            .map_err(|_| Status::failed_precondition("retirement unresolved or claim changed"))?;
        // Completed proof needs no guest. New/unfinished reclamation still requires
        // the original live boot; destruction and previous epochs are separate paths.
        let client = if retirement.completed {
            None
        } else {
            if record.stopped || record.released {
                return Err(Status::failed_precondition(
                    "retirement requires original live guest",
                ));
            }
            let client = record
                .manifest
                .as_ref()
                .ok_or_else(|| uncertain("guest manifest missing"))?
                .guest_client()
                .map_err(uncertain)?;
            retirement
                .barrier
                .validate(client.context(), retirement.barrier.domain)
                .map_err(|_| Status::failed_precondition("retirement guest identity changed"))?;
            Some(client)
        };
        let _file_io = if retirement.barrier.domain == Domain::Files && !retirement.completed {
            Some(
                record
                    .file_io
                    .try_acquire()
                    .map_err(|_| Status::resource_exhausted("allocation file worker busy"))?,
            )
        } else {
            None
        };
        {
            let mut j = self.journal()?;
            // Reject capacity before mutating/poisoning the journal or contacting the guest.
            let before = serde_json::to_vec(
                j.records
                    .get(&owner.allocation_id)
                    .ok_or_else(|| uncertain("allocation missing"))?,
            )
            .map_err(uncertain)?
            .len();
            let after = serde_json::to_vec(&record).map_err(uncertain)?.len();
            let total = serde_json::to_vec(&*j).map_err(uncertain)?.len();
            if total.saturating_sub(before).saturating_add(after) > journal::MAX_BYTES as usize {
                return Err(Status::resource_exhausted(
                    "host journal lacks retirement headroom",
                ));
            }
            // Merge only this field: other allocations may progress.
            let retained = j
                .records
                .get_mut(&owner.allocation_id)
                .ok_or_else(|| uncertain("allocation missing"))?;
            *slot_mut(retained, retirement.barrier.domain) = Some(retirement.clone());
            self.save(&mut j)?;
        }
        if let Some(client) = client {
            deadline(owner.claim_expires_unix_ms)?;
            tokio::runtime::Handle::current().block_on(async {
                tokio::time::timeout(
                    Duration::from_secs(3),
                    client.retire_history(&retirement.barrier),
                )
                .await
                .map_err(uncertain)?
                .map_err(uncertain)
            })?;
            // A timeout or expired claim preserves the floor and charged records for reconciliation.
            deadline(owner.claim_expires_unix_ms)?;
            let mut j = self.journal()?;
            let retained = j
                .records
                .get_mut(&owner.allocation_id)
                .ok_or_else(|| uncertain("allocation missing"))?;
            complete(retained, &retirement).map_err(uncertain)?;
            self.save(&mut j)?;
        }
        deadline(owner.claim_expires_unix_ms)?;
        Ok(HistoryObservation {
            request: Some(request),
            completed: Some((&retirement.barrier).into()),
            simulated: false,
            observed_unix_ms: guardian::wall_ms(),
        })
    }
}

#[cfg(test)]
#[path = "history_tests.rs"]
mod tests;

pub(super) fn closure(record: &Record, domain: Domain) -> anyhow::Result<Option<OperationId>> {
    let live = slot(record, domain);
    let released = super::released_history::through(record, domain);
    let complete = live
        .as_ref()
        .filter(|h| h.completed)
        .map(|h| h.barrier.through)
        .into_iter()
        .chain(released)
        .max();
    anyhow::ensure!(
        live.as_ref()
            .is_none_or(|h| complete.is_some_and(|c| h.barrier.through <= c)),
        "history closure remains pending"
    );
    Ok(complete)
}
