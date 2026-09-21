//! Explicitly simulated command outcomes. No argv is ever executed here.
use super::*;
use sandbox_protocol::{
    command::{CommandRecord, MAX_COMMANDS, MAX_DURATION_MS},
    guest_model as m,
    supervisor::{CommandInspection, CommandObservation, CommandRequest},
};

impl FakeHost {
    pub async fn total_commands(&self) -> u64 {
        self.state.lock().await.total_commands
    }
    pub async fn hold_next_command(&self) {
        self.state.lock().await.hold_next_command = true;
    }
    pub async fn lose_next_command_reply(&self) {
        self.state.lock().await.lose_next_command_reply = true;
    }
    pub async fn finish_command(&self, id: OperationId, exit: i32) -> Result<(), Status> {
        if !(0..=255).contains(&exit) {
            return Err(Status::invalid_argument("invalid exit"));
        }
        let mut state = self.state.lock().await;
        let command = state
            .fences
            .values_mut()
            .find_map(|f| f.commands.get_mut(&id.to_string()))
            .ok_or_else(|| Status::not_found("simulated command missing"))?;
        let receipt = command
            .receipt
            .as_mut()
            .ok_or_else(|| Status::failed_precondition("no simulated receipt"))?;
        if receipt.state != m::State::LaunchIntent {
            return Err(Status::failed_precondition("command already settled"));
        }
        receipt.state = m::State::Exited;
        receipt.exit_code = Some(exit);
        receipt.cleanup_confirmed = true;
        Ok(())
    }
    async fn command(
        &self,
        ownership: Option<Ownership>,
        payload: Option<m::Execute>,
        digest: [u8; 32],
    ) -> Result<CommandObservation, Status> {
        let mut state = self.state.lock().await;
        Self::expire(&mut state);
        let now = unix_ms()?;
        let owner = self.ownership(ownership, now)?;
        if let Some(p) = &payload
            && (p.operation_id.to_string() != owner.operation_id
                || p.output_limit > sandbox_protocol::command::MAX_OUTPUT)
        {
            return Err(Status::invalid_argument("command identity or output cap"));
        }
        if state.fences.get(&owner.allocation_id).is_some_and(|f| {
            !f.commands.contains_key(&owner.operation_id) && f.commands.len() >= MAX_COMMANDS
        }) {
            return Err(Status::resource_exhausted("fake command journal full"));
        }
        Self::fence(&mut state, &owner)?;
        let ready = state
            .allocations
            .get(&owner.allocation_id)
            .is_some_and(|a| a.state == AllocationState::Ready);
        let held = state.hold_next_command;
        let fence = state
            .fences
            .get_mut(&owner.allocation_id)
            .ok_or_else(|| Status::internal("missing fence"))?;
        if let Some(record) = fence.commands.get_mut(&owner.operation_id) {
            if record.digest != digest {
                return Err(Status::already_exists("command changed"));
            }
            if !record.finished()
                && (fence.stopped || !ready)
                && let Some(receipt) = record.receipt.as_mut()
            {
                receipt.state = m::State::Unknown;
            }
            if ready
                && !fence.stopped
                && let Some(receipt) = record.receipt.as_mut()
                && receipt.state == m::State::LaunchIntent
                && receipt.deadline_unix_ms <= now
            {
                receipt.state = m::State::TimedOut;
                receipt.cleanup_confirmed = true;
            }
            return Ok(record.observation(owner, true, now));
        }
        let eligible = !fence.stopped
            && ready
            && fence.commands.values().all(CommandRecord::finished)
            && payload.as_ref().is_some_and(|p| {
                (1..=MAX_DURATION_MS).contains(&p.deadline_unix_ms.saturating_sub(now))
                    && fence
                        .commands
                        .values()
                        .map(|c| c.output_limit)
                        .sum::<u64>()
                        .saturating_add(p.output_limit)
                        <= m::MAX_RESERVED_OUTPUT
            });
        let mut record = CommandRecord::fenced(digest);
        if eligible && let Some(payload) = payload {
            let context = m::Context {
                allocation_id: owner
                    .allocation_id
                    .parse()
                    .map_err(|_| Status::invalid_argument("allocation"))?,
                generation: owner.generation,
                boot_id: "simulated-guest-boot".into(),
            };
            record = CommandRecord::pending(&payload, context.clone())
                .map_err(|_| Status::invalid_argument("command"))?;
            record.receipt = Some(m::Receipt {
                version: 1,
                context,
                operation_id: payload.operation_id,
                digest,
                state: if held {
                    m::State::LaunchIntent
                } else {
                    m::State::Exited
                },
                deadline_unix_ms: payload.deadline_unix_ms,
                output_limit: payload.output_limit,
                cancel_requested: false,
                cleanup_confirmed: !held,
                exit_code: if held { None } else { Some(0) },
                signal: None,
                stdout: m::Output::default(),
                stderr: m::Output::default(),
                reason: None,
            });
        }
        let observation = record.observation(owner.clone(), true, now);
        fence.commands.insert(owner.operation_id, record);
        if eligible {
            state.hold_next_command = false;
            state.total_commands += 1;
            if std::mem::take(&mut state.lose_next_command_reply) {
                return Err(Status::unavailable("injected lost command reply"));
            }
        }
        Ok(observation)
    }
    pub(super) async fn execute_command_inner(
        &self,
        r: CommandRequest,
    ) -> Result<CommandObservation, Status> {
        let payload: m::Execute = r
            .command
            .ok_or_else(|| Status::invalid_argument("command required"))?
            .try_into()
            .map_err(|_| Status::invalid_argument("invalid command"))?;
        let digest = payload
            .digest()
            .map_err(|_| Status::invalid_argument("invalid command digest"))?;
        self.command(r.ownership, Some(payload), digest).await
    }
    pub(super) async fn inspect_command_inner(
        &self,
        r: CommandInspection,
    ) -> Result<CommandObservation, Status> {
        let digest: [u8; 32] = r
            .command_digest
            .try_into()
            .map_err(|_| Status::invalid_argument("invalid command digest"))?;
        self.command(r.ownership, None, digest).await
    }
}
