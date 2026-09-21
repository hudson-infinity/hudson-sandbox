//! Simulation has explicit empty output, never invented real execution.
use super::*;
use sandbox_protocol::{
    guest as w,
    supervisor::{LiveOutputObservation, LiveOutputRequest, live_output_server::LiveOutput},
};
use sandbox_supervisor::live_output as live;
impl FakeHost {
    async fn live_read(&self, request: LiveOutputRequest) -> Result<LiveOutputObservation, Status> {
        let _permit = self
            .output_readers
            .try_acquire()
            .map_err(|_| Status::resource_exhausted("live output readers busy"))?;
        let (scope, read) = live::decode(&request, unix_ms()?)?;
        if scope.owner.host_id != self.config.host || scope.owner.host_epoch != self.config.epoch {
            return Err(Status::failed_precondition("wrong live output host epoch"));
        }
        // Read does not expire allocations, create fences, or change command receipts.
        let state = self.state.lock().await;
        let fence = state
            .fences
            .get(&scope.owner.allocation_id.to_string())
            .ok_or_else(|| Status::not_found("fake allocation missing"))?;
        let command = fence
            .commands
            .get(&scope.owner.operation_id.to_string())
            .ok_or_else(|| Status::not_found("fake command missing"))?;
        live::validate_owner(&scope, &fence.owner, command)?;
        let allocation = state
            .allocations
            .get(&scope.owner.allocation_id.to_string())
            .filter(|a| a.state == AllocationState::Ready && a.expires > Instant::now());
        if fence.stopped || allocation.is_none() {
            return Err(Status::unavailable("fake guest history unavailable"));
        }
        let receipt = command
            .receipt
            .clone()
            .ok_or_else(|| Status::unavailable("fake receipt missing"))?;
        if read.offset != 0 {
            return Err(Status::out_of_range("fake output is empty"));
        }
        let chunk = w::OutputChunk {
            operation_id: read.operation_id.clone(),
            stream: read.stream,
            offset: 0,
            data: vec![],
            next_offset: 0,
            at_end: true,
            complete: command.finished(),
        };
        live::validate_chunk(&scope, &read, command, &chunk, &receipt)?;
        live::observation(
            request,
            self.config.host.to_string(),
            self.config.epoch,
            true,
            unix_ms()?,
            chunk,
            receipt,
        )
    }
}

#[tonic::async_trait]
impl LiveOutput for FakeHost {
    async fn read(
        &self,
        request: Request<LiveOutputRequest>,
    ) -> Result<Response<LiveOutputObservation>, Status> {
        tokio::time::timeout(live::READ_TIMEOUT, self.live_read(request.into_inner()))
            .await
            .map_err(|_| Status::unavailable("live output read timed out"))?
            .map(Response::new)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use sandbox_protocol::Id;
    #[tokio::test(start_paused = true)]
    async fn reader_capacity_is_bounded_and_recovers_after_cancellation() {
        let host = FakeHost::new(FakeConfig {
            host: HostId::generate(),
            epoch: 1,
            images: BTreeSet::from([format!("sha256:{}", "a".repeat(64))]),
            capacity: Resources {
                vcpu: 4,
                memory_mib: 8192,
                disk_mib: 65536,
            },
        })
        .unwrap();
        let scope = sandbox_protocol::live_output::LiveOutputScope {
            version: 1,
            owner: sandbox_protocol::output::OutputOwner {
                project_id: ProjectId::generate(),
                sandbox_id: SandboxId::generate(),
                operation_id: OperationId::generate(),
                allocation_id: AllocationId::generate(),
                host_id: host.config.host,
                host_epoch: 1,
                generation: 1,
                boot_id: "boot".into(),
            },
            command_digest: [0; 32],
            output_limit: 32,
            deadline_unix_ms: unix_ms().unwrap() + 20000,
        };
        let request = LiveOutputRequest {
            scope_json: serde_json::to_vec(&scope).unwrap(),
            output: Some(w::ReadOutput {
                operation_id: scope.owner.operation_id.to_string(),
                stream: w::Stream::Stdout as i32,
                offset: 0,
                limit: 32,
            }),
            expires_unix_ms: unix_ms().unwrap() + 30000,
        };
        // Stall metadata while actual calls hold the four reader slots.
        let state = host.state.lock().await;
        let mut tasks = Vec::new();
        for _ in 0..4 {
            let h = host.clone();
            let r = request.clone();
            tasks.push(tokio::spawn(async move { h.read(Request::new(r)).await }));
        }
        tokio::task::yield_now().await;
        assert_eq!(host.output_readers.available_permits(), 0);
        assert_eq!(
            host.read(Request::new(request.clone()))
                .await
                .unwrap_err()
                .code(),
            tonic::Code::ResourceExhausted
        );
        let cancelled = tasks.pop().unwrap();
        cancelled.abort();
        assert!(cancelled.await.unwrap_err().is_cancelled());
        assert_eq!(host.output_readers.available_permits(), 1);
        tokio::time::advance(live::READ_TIMEOUT).await;
        for task in tasks {
            assert_eq!(
                task.await.unwrap().unwrap_err().code(),
                tonic::Code::Unavailable
            );
        }
        assert_eq!(host.output_readers.available_permits(), 4);
        drop(state);
        assert_eq!(
            host.read(Request::new(request)).await.unwrap_err().code(),
            tonic::Code::NotFound
        );
        assert_eq!(host.total_commands().await, 0);
    }
}
