use super::*;
use crate::archive::{self as a, ArchiveRecord, GuestOutput, OutputSource};
use sandbox_protocol::{
    output::{OutputPlans, OutputTicket},
    supervisor::{OutputObservation, OutputRequest},
};

pub(super) fn validate_owner(owner: &Ownership, ticket: &OutputTicket) -> Result<(), Status> {
    let o = &ticket.owner;
    if owner.host_id != o.host_id.to_string()
        || owner.project_id != o.project_id.to_string()
        || owner.sandbox_id != o.sandbox_id.to_string()
        || owner.allocation_id != o.allocation_id.to_string()
        || owner.generation != o.generation
        || owner.supervisor_epoch != o.host_epoch
    {
        return Err(Status::failed_precondition(
            "archive allocation ownership changed",
        ));
    }
    Ok(())
}
struct Prepared {
    record: ArchiveRecord,
    receipt: sandbox_protocol::guest_model::Receipt,
    source: Option<GuestOutput>,
}

impl Host {
    // Only short journal/guardian setup uses lifecycle workers. Network reads
    // and object writes run without the allocation gate or journal mutex.
    fn output_setup(&self, request: &OutputRequest) -> Result<Prepared, Status> {
        let (ticket, plans) = a::decode(request, guardian::wall_ms())?;
        if self.inner.artifacts.is_none() {
            return Err(Status::failed_precondition(
                "output storage is not configured",
            ));
        }
        if ticket.owner.host_id != self.inner.config.host {
            return Err(Status::failed_precondition("wrong output host"));
        }
        let id = ticket.owner.allocation_id.to_string();
        let op = ticket.owner.operation_id.to_string();
        let gate = self
            .journal()?
            .records
            .get(&id)
            .ok_or_else(|| Status::not_found("allocation history missing"))?
            .gate
            .clone();
        let _guard = lock(&gate)?;
        let (record, manifest) = {
            let mut j = self.journal()?;
            let r = j
                .records
                .get_mut(&id)
                .ok_or_else(|| uncertain("allocation missing"))?;
            validate_owner(&r.owner, &ticket)?;
            history::check(
                r,
                sandbox_protocol::history::Domain::Commands,
                ticket.owner.operation_id,
            )?;
            let command = r
                .commands
                .get(&op)
                .ok_or_else(|| Status::not_found("command history missing"))?;
            let receipt = command
                .receipt
                .as_ref()
                .ok_or_else(|| Status::failed_precondition("final receipt missing"))?;
            command
                .validate_receipt(ticket.owner.operation_id, receipt)
                .map_err(uncertain)?;
            a::validate_receipt(&ticket, receipt)?;
            if let Some(p) = &plans {
                a::validate_plans(&ticket, p, receipt)?;
            }
            let old = r.archives.get(&op);
            if let Some(old) = old {
                if old.ticket != ticket
                    || request.publication_revision < old.revision
                    || plans
                        .as_ref()
                        .is_some_and(|p| old.plans.as_ref() != Some(p))
                {
                    return Err(Status::failed_precondition(
                        "publication ticket, plan or revision changed",
                    ));
                }
            } else if plans.is_some() {
                return Err(Status::failed_precondition("output must be prepared first"));
            }
            let archive = ArchiveRecord {
                ticket: ticket.clone(),
                plans: old.and_then(|r| r.plans.clone()),
                revision: request.publication_revision,
            };
            let receipt = receipt.clone();
            // Old epochs may reconcile S3 but can never contact or restart a VM.
            let manifest = if !r.stopped && r.owner.supervisor_epoch == self.inner.config.epoch {
                r.manifest.clone()
            } else {
                None
            };
            r.archives.insert(op.clone(), archive.clone());
            self.save(&mut j)?;
            ((archive, receipt), manifest)
        };
        a::decode(request, guardian::wall_ms())?; // Recheck after durable I/O.
        drop(_guard);
        let source = manifest
            .and_then(|m| m.guest_client().ok())
            .filter(|client| client.context().boot_id == ticket.owner.boot_id)
            .map(|client| GuestOutput {
                client,
                operation: ticket.owner.operation_id,
            });
        Ok(Prepared {
            record: record.0,
            receipt: record.1,
            source,
        })
    }
    fn output_finish(&self, request: &OutputRequest, plans: &OutputPlans) -> Result<(), Status> {
        let (ticket, _) = a::decode(request, guardian::wall_ms())?;
        let id = ticket.owner.allocation_id.to_string();
        let op = ticket.owner.operation_id.to_string();
        let gate = self
            .journal()?
            .records
            .get(&id)
            .ok_or_else(|| uncertain("allocation missing"))?
            .gate
            .clone();
        let _guard = lock(&gate)?;
        let mut j = self.journal()?;
        let r = j
            .records
            .get_mut(&id)
            .ok_or_else(|| uncertain("allocation missing"))?;
        validate_owner(&r.owner, &ticket)?;
        history::check(
            r,
            sandbox_protocol::history::Domain::Commands,
            ticket.owner.operation_id,
        )?;
        let receipt = r
            .commands
            .get(&op)
            .and_then(|c| c.receipt.as_ref())
            .ok_or_else(|| uncertain("receipt missing"))?;
        a::validate_plans(&ticket, plans, receipt)?;
        let saved = r
            .archives
            .get_mut(&op)
            .ok_or_else(|| uncertain("archive missing"))?;
        if saved.ticket != ticket
            || saved.revision != request.publication_revision
            || saved.plans.as_ref().is_some_and(|p| p != plans)
        {
            return Err(Status::failed_precondition("publication ownership changed"));
        }
        saved.plans = Some(plans.clone());
        self.save(&mut j)?;
        a::decode(request, guardian::wall_ms())?;
        Ok(())
    }
    pub(super) async fn prepare_output_inner(
        &self,
        request: OutputRequest,
    ) -> Result<OutputObservation, Status> {
        if !request.plans_json.is_empty() {
            return Err(Status::invalid_argument("prepare does not accept plans"));
        }
        let _permit = self
            .inner
            .archive_workers
            .try_acquire()
            .map_err(|_| Status::resource_exhausted("archive workers busy"))?;
        let req = request.clone();
        let prepared = self.work(move |h| h.output_setup(&req)).await?;
        let plans = if let Some(plans) = prepared.record.plans {
            plans
        } else {
            a::collect(
                &prepared.record.ticket,
                &prepared.receipt,
                prepared
                    .source
                    .as_ref()
                    .ok_or_else(|| Status::unavailable("guest output history unavailable"))?,
            )
            .await?
            .plans
        };
        let req = request.clone();
        let p = plans.clone();
        self.work(move |h| h.output_finish(&req, &p)).await?;
        a::observation(
            request,
            self.inner.config.host.to_string(),
            self.inner.config.epoch,
            false,
            guardian::wall_ms(),
            &plans,
            None,
        )
    }
    pub(super) async fn archive_output_inner(
        &self,
        request: OutputRequest,
    ) -> Result<OutputObservation, Status> {
        if request.plans_json.is_empty() {
            return Err(Status::invalid_argument("archive requires persisted plans"));
        }
        let _permit = self
            .inner
            .archive_workers
            .try_acquire()
            .map_err(|_| Status::resource_exhausted("archive workers busy"))?;
        let req = request.clone();
        let prepared = self.work(move |h| h.output_setup(&req)).await?;
        let plans = prepared
            .record
            .plans
            .as_ref()
            .ok_or_else(|| Status::failed_precondition("prepared plans missing"))?;
        let store = self
            .inner
            .artifacts
            .as_ref()
            .ok_or_else(|| Status::failed_precondition("output storage not configured"))?;
        let refs = tokio::time::timeout(
            a::ARCHIVE_TIMEOUT,
            a::upload(
                store,
                &prepared.record.ticket,
                plans,
                &prepared.receipt,
                prepared.source.as_ref().map(|s| s as &dyn OutputSource),
                || {
                    let now = guardian::wall_ms();
                    if now >= request.claim_expires_unix_ms {
                        i64::MAX
                    } else {
                        now
                    }
                },
            ),
        )
        .await
        .map_err(|_| Status::unavailable("output archive timed out"))??;
        let req = request.clone();
        let p = plans.clone();
        self.work(move |h| h.output_finish(&req, &p)).await?;
        a::observation(
            request,
            self.inner.config.host.to_string(),
            self.inner.config.epoch,
            false,
            guardian::wall_ms(),
            plans,
            Some(&refs),
        )
    }
}
