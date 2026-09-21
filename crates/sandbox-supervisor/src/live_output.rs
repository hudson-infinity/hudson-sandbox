//! Read-only scope and byte validation shared by real and simulated hosts.
use sandbox_protocol::{
    command::CommandRecord,
    guest as w, guest_model as m,
    live_output::LiveOutputScope,
    output::MAX_CHUNK,
    supervisor::{LiveOutputObservation, LiveOutputRequest, Ownership},
};
use tonic::Status;
pub const READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(4);
pub fn decode(
    request: &LiveOutputRequest,
    now: i64,
) -> Result<(LiveOutputScope, w::ReadOutput), Status> {
    if request.scope_json.len() > 8192
        || !(1..=30_000).contains(&request.expires_unix_ms.saturating_sub(now))
    {
        return Err(Status::invalid_argument(
            "invalid live output scope or deadline",
        ));
    }
    let scope: LiveOutputScope = serde_json::from_slice(&request.scope_json)
        .map_err(|_| Status::invalid_argument("invalid live output scope"))?;
    scope
        .validate()
        .map_err(|_| Status::invalid_argument("invalid live output scope"))?;
    let read = request
        .output
        .clone()
        .ok_or_else(|| Status::invalid_argument("output selector required"))?;
    if read.operation_id != scope.owner.operation_id.to_string()
        || !(1..=MAX_CHUNK as u32).contains(&read.limit)
        || read.offset > scope.output_limit
        || !matches!(
            w::Stream::try_from(read.stream),
            Ok(w::Stream::Stdout | w::Stream::Stderr)
        )
    {
        return Err(Status::invalid_argument(
            "invalid live output range or identity",
        ));
    }
    Ok((scope, read))
}
pub fn validate_owner(
    scope: &LiveOutputScope,
    owner: &Ownership,
    command: &CommandRecord,
) -> Result<(), Status> {
    let o = &scope.owner;
    if o.project_id.to_string() != owner.project_id
        || o.sandbox_id.to_string() != owner.sandbox_id
        || o.allocation_id.to_string() != owner.allocation_id
        || o.host_id.to_string() != owner.host_id
        || o.host_epoch != owner.supervisor_epoch
        || o.generation != owner.generation
        || command.not_started
        || command.digest != scope.command_digest
        || command.output_limit != scope.output_limit
        || command.deadline_unix_ms != scope.deadline_unix_ms
        || command.context.as_ref().is_none_or(|c| {
            c.allocation_id != o.allocation_id
                || c.generation != o.generation
                || c.boot_id != o.boot_id
        })
    {
        return Err(Status::failed_precondition(
            "live output ownership mismatch",
        ));
    }
    Ok(())
}
pub fn validate_chunk(
    scope: &LiveOutputScope,
    read: &w::ReadOutput,
    command: &CommandRecord,
    chunk: &w::OutputChunk,
    receipt: &m::Receipt,
) -> Result<(), Status> {
    command
        .validate_receipt(scope.owner.operation_id, receipt)
        .map_err(|_| Status::unavailable("invalid live output receipt"))?;
    if receipt.state == m::State::Unknown {
        return Err(Status::unavailable("guest output state unknown"));
    }
    let stats = match w::Stream::try_from(read.stream) {
        Ok(w::Stream::Stdout) => &receipt.stdout,
        Ok(w::Stream::Stderr) => &receipt.stderr,
        _ => return Err(Status::invalid_argument("invalid stream")),
    };
    let final_receipt = receipt.cleanup_confirmed
        && matches!(
            receipt.state,
            m::State::Exited | m::State::TimedOut | m::State::Cancelled
        );
    if chunk.operation_id != read.operation_id
        || chunk.stream != read.stream
        || chunk.offset != read.offset
        || chunk.data.len() > read.limit as usize
        || chunk.data.len() > MAX_CHUNK
        || chunk.offset.checked_add(chunk.data.len() as u64) != Some(chunk.next_offset)
        || chunk.next_offset > stats.stored
        || (!chunk.at_end && (chunk.data.is_empty() || chunk.next_offset >= stats.stored))
        || (chunk.complete
            && (!final_receipt || chunk.at_end != (chunk.next_offset == stats.stored)))
    {
        return Err(Status::unavailable("invalid live output bytes or bounds"));
    }
    Ok(())
}
pub fn observation(
    request: LiveOutputRequest,
    host: String,
    epoch: i64,
    simulated: bool,
    now: i64,
    chunk: w::OutputChunk,
    receipt: m::Receipt,
) -> Result<LiveOutputObservation, Status> {
    decode(&request, now)?;
    Ok(LiveOutputObservation {
        request: Some(request),
        host_id: host,
        supervisor_epoch: epoch,
        simulated,
        observed_unix_ms: now,
        chunk: Some(chunk),
        receipt: Some((&receipt).into()),
    })
}
