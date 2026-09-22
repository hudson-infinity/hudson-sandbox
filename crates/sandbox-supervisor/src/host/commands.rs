//! One host-to-guest dispatch at most; every retry reconciles the retained record.
use super::*;
use sandbox_protocol::{
    command::{CommandRecord, MAX_COMMANDS, MAX_DURATION_MS},
    guest_model as m,
    supervisor::{CommandInspection, CommandObservation, CommandRequest},
};

impl Host {
    fn save_command(&self, owner: &Ownership, command: CommandRecord) -> Result<(), Status> {
        let mut j = self.journal()?;
        j.records
            .get_mut(&owner.allocation_id)
            .ok_or_else(|| uncertain("missing allocation"))?
            .commands
            .insert(owner.operation_id.clone(), command);
        self.save(&mut j)
    }
    fn command_gate(&self, owner: &Ownership) -> Result<Arc<Mutex<()>>, Status> {
        let gate = self.gate(owner)?;
        let j = self.journal()?;
        let r = j
            .records
            .get(&owner.allocation_id)
            .ok_or_else(|| uncertain("missing allocation"))?;
        if r.files.contains_key(&owner.operation_id) {
            return Err(Status::already_exists(
                "operation belongs to a file transfer",
            ));
        }
        if !r.commands.contains_key(&owner.operation_id) && r.commands.len() >= MAX_COMMANDS {
            return Err(Status::resource_exhausted("retained command journal full"));
        }
        Ok(gate)
    }
    // Call only while holding the allocation gate. Admission limits must be
    // checked before adding a revision fence so command pressure cannot consume
    // the slots reserved for destroy and other lifecycle operations.
    fn command_fence(&self, owner: &Ownership) -> Result<Record, Status> {
        {
            let j = self.journal()?;
            let r = j
                .records
                .get(&owner.allocation_id)
                .ok_or_else(|| uncertain("missing allocation"))?;
            history::check(
                r,
                sandbox_protocol::history::Domain::Commands,
                owner.operation_id.parse().map_err(uncertain)?,
            )?;
            if r.files.contains_key(&owner.operation_id) {
                return Err(Status::already_exists(
                    "operation belongs to a file transfer",
                ));
            }
            if !r.commands.contains_key(&owner.operation_id) && r.commands.len() >= MAX_COMMANDS {
                return Err(Status::resource_exhausted("retained command journal full"));
            }
        }
        self.fence(owner)
    }
    fn observe_command(
        &self,
        owner: Ownership,
        record: &Record,
        mut command: CommandRecord,
        cancel: bool,
    ) -> Result<CommandObservation, Status> {
        let mut current = command.finished();
        if !command.finished()
            && !record.stopped
            && let Some(manifest) = &record.manifest
        {
            // A failed or missing guest response never authorizes another Execute.
            if let Ok(client) = manifest.guest_client() {
                let id = owner
                    .operation_id
                    .parse::<OperationId>()
                    .map_err(uncertain)?;
                let response = if cancel {
                    guest_call(client.cancel(id))
                } else {
                    guest_call(client.inspect(id))
                };
                if let Ok(receipt) = response {
                    command.validate_receipt(id, &receipt).map_err(uncertain)?;
                    command.receipt = Some(receipt);
                    current = true;
                    self.save_command(&owner, command.clone())?;
                }
            }
        }
        let mut observation = command.observation(owner, false, guardian::wall_ms());
        if !current {
            observation.receipt = None;
        }
        Ok(observation)
    }
    pub(super) fn inspect_command_sync(
        &self,
        request: CommandInspection,
    ) -> Result<CommandObservation, Status> {
        self.inspect_or_cancel(request, false)
    }
    pub(super) fn cancel_command_sync(
        &self,
        request: CommandInspection,
    ) -> Result<CommandObservation, Status> {
        self.inspect_or_cancel(request, true)
    }
    fn inspect_or_cancel(
        &self,
        request: CommandInspection,
        cancel: bool,
    ) -> Result<CommandObservation, Status> {
        let owner = self.owner(request.ownership)?;
        let digest: [u8; 32] = request
            .command_digest
            .try_into()
            .map_err(|_| Status::invalid_argument("invalid command digest"))?;
        let gate = self.command_gate(&owner)?;
        let _guard = lock(&gate)?;
        let record = self.command_fence(&owner)?;
        let command = match record.commands.get(&owner.operation_id) {
            Some(command) if command.digest == digest => command.clone(),
            Some(_) => return Err(Status::already_exists("command digest changed")),
            None => {
                if record.commands.len() >= MAX_COMMANDS {
                    return Err(Status::resource_exhausted("command journal full"));
                }
                // This same epoch has no dispatch intent. Commit a tombstone so a
                // delayed Execute cannot arrive after this absence observation.
                let command = CommandRecord::fenced(digest);
                self.save_command(&owner, command.clone())?;
                command
            }
        };
        self.observe_command(owner, &record, command, cancel)
    }
    pub(super) fn execute_command_sync(
        &self,
        request: CommandRequest,
    ) -> Result<CommandObservation, Status> {
        let owner = self.owner(request.ownership)?;
        let command: m::Execute = request
            .command
            .ok_or_else(|| Status::invalid_argument("command required"))?
            .try_into()
            .map_err(|_| Status::invalid_argument("invalid command"))?;
        if command.operation_id.to_string() != owner.operation_id
            || command.output_limit > sandbox_protocol::command::MAX_OUTPUT
        {
            return Err(Status::invalid_argument(
                "command ownership or output budget mismatch",
            ));
        }
        let digest = command.digest().map_err(uncertain)?;
        let gate = self.command_gate(&owner)?;
        let _guard = lock(&gate)?;
        let record = self.command_fence(&owner)?;
        if let Some(previous) = record.commands.get(&owner.operation_id) {
            if previous.digest != digest {
                return Err(Status::already_exists("command retry changed payload"));
            }
            return self.observe_command(owner, &record, previous.clone(), false);
        }
        if record.commands.len() >= MAX_COMMANDS {
            return Err(Status::resource_exhausted("command journal full"));
        }
        let remaining = command
            .deadline_unix_ms
            .checked_sub(guardian::wall_ms())
            .unwrap_or(0);
        let reserved: u64 = record.commands.values().map(|c| c.output_limit).sum();
        let eligible = !record.stopped
            && !record.released
            && record.commands.values().all(CommandRecord::finished)
            && (1..=MAX_DURATION_MS).contains(&remaining)
            && reserved.saturating_add(command.output_limit) <= m::MAX_RESERVED_OUTPUT;
        let client = if eligible {
            record.manifest.as_ref().and_then(|m| m.guest_client().ok())
        } else {
            None
        };
        let Some(client) = client else {
            let command = CommandRecord::fenced(digest);
            self.save_command(&owner, command.clone())?;
            return Ok(command.observation(owner, false, guardian::wall_ms()));
        };
        deadline(owner.claim_expires_unix_ms)?;
        let mut retained =
            CommandRecord::pending(&command, client.context().clone()).map_err(uncertain)?;
        self.save_command(&owner, retained.clone())?;
        // A journal write may stall past the controller's authority. Keep the
        // intent uncertain, but never send new work from an expired claimant.
        deadline(owner.claim_expires_unix_ms)?;
        // Intent survives the RPC task and response. Neither transport nor guest
        // errors prove that dispatch did not happen after this point.
        if let Ok(receipt) = guest_call(client.execute(&command)) {
            retained
                .validate_receipt(command.operation_id, &receipt)
                .map_err(uncertain)?;
            retained.receipt = Some(receipt);
            self.save_command(&owner, retained.clone())?;
        }
        Ok(retained.observation(owner, false, guardian::wall_ms()))
    }
}
fn guest_call<T>(
    future: impl std::future::Future<Output = anyhow::Result<T>>,
) -> anyhow::Result<T> {
    // This runs inside the host's bounded blocking worker, never its async runtime.
    // Releasing this gate within three seconds leaves room for lease maintenance.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async { tokio::time::timeout(Duration::from_secs(3), future).await? })
}
