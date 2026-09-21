//! Bounded final-output collection. The lifecycle executor is never invoked.
use sandbox_protocol::{
    guest as w,
    guest_model::Receipt,
    output::{MAX_CHUNK, OutputName, OutputPlan, OutputPlans, OutputRefs, OutputTicket},
    supervisor::{OutputObservation, OutputRequest},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{fmt, time::Duration};
use tonic::Status;

pub const MAX_METADATA: usize = 8192;
pub const CAPTURE_TIMEOUT: Duration = Duration::from_secs(20);
pub const ARCHIVE_TIMEOUT: Duration = Duration::from_secs(75);

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArchiveRecord {
    pub ticket: OutputTicket,
    pub plans: Option<OutputPlans>,
    pub revision: i64,
}

pub fn decode(
    request: &OutputRequest,
    now: i64,
) -> Result<(OutputTicket, Option<OutputPlans>), Status> {
    if request.publication_revision <= 0
        || !(1..=300_000).contains(&request.claim_expires_unix_ms.saturating_sub(now))
        || request.ticket_json.len() > MAX_METADATA
        || request.plans_json.len() > MAX_METADATA
    {
        return Err(Status::invalid_argument(
            "invalid publication claim or metadata bounds",
        ));
    }
    let ticket: OutputTicket = serde_json::from_slice(&request.ticket_json)
        .map_err(|_| Status::invalid_argument("invalid output ticket"))?;
    ticket
        .validate()
        .map_err(|_| Status::invalid_argument("invalid output ticket"))?;
    if ticket.created_unix_ms > now || ticket.expires_unix_ms <= now {
        return Err(Status::failed_precondition(
            "output retention is not current",
        ));
    }
    let plans = if request.plans_json.is_empty() {
        None
    } else {
        let plans: OutputPlans = serde_json::from_slice(&request.plans_json)
            .map_err(|_| Status::invalid_argument("invalid output plans"))?;
        ticket
            .validate_plans(&plans)
            .map_err(|_| Status::invalid_argument("output plans do not match ticket"))?;
        Some(plans)
    };
    Ok((ticket, plans))
}
pub fn validate_receipt(ticket: &OutputTicket, receipt: &Receipt) -> Result<(), Status> {
    use sandbox_protocol::guest_model::State;
    receipt
        .validate()
        .map_err(|_| Status::failed_precondition("invalid retained receipt"))?;
    if receipt.operation_id != ticket.owner.operation_id
        || receipt.context.allocation_id != ticket.owner.allocation_id
        || receipt.context.generation != ticket.owner.generation
        || receipt.context.boot_id != ticket.owner.boot_id
        || receipt.output_limit != ticket.output_limit
        || !receipt.cleanup_confirmed
        || !matches!(
            receipt.state,
            State::Exited | State::TimedOut | State::Cancelled
        )
    {
        return Err(Status::failed_precondition(
            "output requires the original final receipt",
        ));
    }
    Ok(())
}
pub fn validate_plans(
    ticket: &OutputTicket,
    plans: &OutputPlans,
    receipt: &Receipt,
) -> Result<(), Status> {
    validate_receipt(ticket, receipt)?;
    ticket
        .validate_plans(plans)
        .map_err(|_| Status::failed_precondition("saved output plan changed"))?;
    for (p, s) in [
        (&plans.stdout, &receipt.stdout),
        (&plans.stderr, &receipt.stderr),
    ] {
        if p.size != s.stored || p.seen != s.seen || p.truncated != s.truncated {
            return Err(Status::failed_precondition("output statistics changed"));
        }
    }
    Ok(())
}
pub fn encode(value: &impl Serialize) -> Result<Vec<u8>, Status> {
    let bytes = serde_json::to_vec(value)
        .map_err(|_| Status::internal("output metadata encoding failed"))?;
    if bytes.len() > MAX_METADATA {
        return Err(Status::resource_exhausted("output metadata too large"));
    }
    Ok(bytes)
}
pub fn observation(
    request: OutputRequest,
    host: String,
    epoch: i64,
    simulated: bool,
    now: i64,
    plans: &OutputPlans,
    refs: Option<&OutputRefs>,
) -> Result<OutputObservation, Status> {
    Ok(OutputObservation {
        request: Some(request),
        host_id: host,
        supervisor_epoch: epoch,
        simulated,
        observed_unix_ms: now,
        plans_json: encode(plans)?,
        references_json: refs.map(encode).transpose()?.unwrap_or_default(),
    })
}

#[tonic::async_trait]
pub trait OutputSource: fmt::Debug + Send + Sync {
    async fn read(
        &self,
        name: OutputName,
        offset: u64,
        limit: u32,
    ) -> Result<w::OutputChunk, Status>;
}
pub struct Captured {
    pub plans: OutputPlans,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}
impl fmt::Debug for Captured {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Captured")
            .field("plans", &self.plans)
            .finish_non_exhaustive()
    }
}

pub async fn collect(
    ticket: &OutputTicket,
    receipt: &Receipt,
    source: &dyn OutputSource,
) -> Result<Captured, Status> {
    ticket
        .validate()
        .map_err(|_| Status::invalid_argument("invalid ticket"))?;
    validate_receipt(ticket, receipt)?;
    tokio::time::timeout(CAPTURE_TIMEOUT, async {
        let (stdout, a) = stream(ticket, OutputName::Stdout, &receipt.stdout, source).await?;
        let (stderr, b) = stream(ticket, OutputName::Stderr, &receipt.stderr, source).await?;
        let plans = OutputPlans {
            stdout: a,
            stderr: b,
        };
        validate_plans(ticket, &plans, receipt)?;
        Ok(Captured {
            stdout,
            stderr,
            plans,
        })
    })
    .await
    .map_err(|_| Status::unavailable("output collection timed out"))?
}

/// Reconcile existing objects before needing guest bytes. A missing object is
/// not an empty stream; without its source this returns explicit unavailability.
pub async fn upload(
    store: &sandbox_artifacts::ArtifactStore,
    ticket: &OutputTicket,
    plans: &OutputPlans,
    receipt: &Receipt,
    source: Option<&dyn OutputSource>,
    now: impl Fn() -> i64,
) -> Result<OutputRefs, Status> {
    validate_plans(ticket, plans, receipt)?;
    let find = |error| match error {
        sandbox_artifacts::Error::Missing => Ok(None),
        _ => Err(Status::unavailable("artifact reconciliation failed")),
    };
    let stdout = match store.reconcile(&plans.stdout, &ticket.owner, now()).await {
        Ok(r) => Some(r),
        Err(e) => find(e)?,
    };
    let stderr = match store.reconcile(&plans.stderr, &ticket.owner, now()).await {
        Ok(r) => Some(r),
        Err(e) => find(e)?,
    };
    if let (Some(stdout), Some(stderr)) = (stdout.as_ref(), stderr.as_ref()) {
        return Ok(OutputRefs {
            stdout: stdout.clone(),
            stderr: stderr.clone(),
        });
    }
    let captured = collect(
        ticket,
        receipt,
        source.ok_or_else(|| {
            Status::unavailable("guest history missing and output archive incomplete")
        })?,
    )
    .await?;
    if &captured.plans != plans {
        return Err(Status::failed_precondition(
            "captured output changed after plan persistence",
        ));
    }
    let stdout = match stdout {
        Some(r) => r,
        None => store
            .upload(&plans.stdout, &ticket.owner, now(), &captured.stdout)
            .await
            .map_err(|_| Status::unavailable("stdout upload uncertain"))?,
    };
    let stderr = match stderr {
        Some(r) => r,
        None => store
            .upload(&plans.stderr, &ticket.owner, now(), &captured.stderr)
            .await
            .map_err(|_| Status::unavailable("stderr upload uncertain"))?,
    };
    Ok(OutputRefs { stdout, stderr })
}
async fn stream(
    ticket: &OutputTicket,
    name: OutputName,
    stats: &sandbox_protocol::guest_model::Output,
    source: &dyn OutputSource,
) -> Result<(Vec<u8>, OutputPlan), Status> {
    let mut bytes = Vec::with_capacity(stats.stored as usize);
    loop {
        let offset = bytes.len() as u64;
        let chunk = source.read(name, offset, MAX_CHUNK as u32).await?;
        let stream = match name {
            OutputName::Stdout => w::Stream::Stdout,
            OutputName::Stderr => w::Stream::Stderr,
        } as i32;
        if chunk.operation_id != ticket.owner.operation_id.to_string()
            || chunk.stream != stream
            || chunk.offset != offset
            || chunk.data.len() > MAX_CHUNK
            || offset.checked_add(chunk.data.len() as u64) != Some(chunk.next_offset)
            || chunk.next_offset > stats.stored
            || !chunk.complete
            || chunk.at_end != (chunk.next_offset == stats.stored)
            || (!chunk.at_end && chunk.data.is_empty())
        {
            return Err(Status::failed_precondition(
                "guest output does not match final receipt",
            ));
        }
        bytes.extend_from_slice(&chunk.data);
        if chunk.at_end {
            break;
        }
    }
    let plan = OutputPlan {
        version: 1,
        owner: ticket.owner.clone(),
        upload_attempt: ticket.upload_attempt,
        name,
        size: stats.stored,
        sha256: hex::encode(Sha256::digest(&bytes)),
        seen: stats.seen,
        truncated: stats.truncated,
        created_unix_ms: ticket.created_unix_ms,
        expires_unix_ms: ticket.expires_unix_ms,
        delete_after_unix_ms: ticket.delete_after_unix_ms,
    };
    Ok((bytes, plan))
}

#[cfg(unix)]
#[derive(Debug)]
pub struct GuestOutput {
    pub client: crate::guest::GuestClient,
    pub operation: sandbox_protocol::OperationId,
}
#[cfg(unix)]
#[tonic::async_trait]
impl OutputSource for GuestOutput {
    async fn read(
        &self,
        name: OutputName,
        offset: u64,
        limit: u32,
    ) -> Result<w::OutputChunk, Status> {
        self.client
            .output(w::ReadOutput {
                operation_id: self.operation.to_string(),
                stream: match name {
                    OutputName::Stdout => w::Stream::Stdout,
                    OutputName::Stderr => w::Stream::Stderr,
                } as i32,
                offset,
                limit,
            })
            .await
            .map_err(|_| Status::unavailable("guest output unavailable; reconcile artifacts"))
    }
}
