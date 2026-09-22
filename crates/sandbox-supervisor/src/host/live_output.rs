use super::*;
use crate::live_output as live;
use sandbox_protocol::{
    command::CommandRecord,
    live_output::LiveOutputScope,
    supervisor::{LiveOutputObservation, LiveOutputRequest, live_output_server::LiveOutput},
};
impl Host {
    fn live_record(
        &self,
        scope: &LiveOutputScope,
    ) -> Result<(CommandRecord, Manifest, readers::Pin), Status> {
        if scope.owner.host_id != self.inner.config.host
            || scope.owner.host_epoch != self.inner.config.epoch
        {
            return Err(Status::failed_precondition(
                "live output requires the producing host epoch",
            ));
        }
        let j = self.journal()?;
        let record = j
            .records
            .get(&scope.owner.allocation_id.to_string())
            .ok_or_else(|| Status::not_found("allocation history missing"))?;
        history::check(
            record,
            sandbox_protocol::history::Domain::Commands,
            scope.owner.operation_id,
        )?;
        let command = record
            .commands
            .get(&scope.owner.operation_id.to_string())
            .ok_or_else(|| Status::not_found("command history missing"))?;
        live::validate_owner(scope, &record.owner, command)?;
        if record.stopped || record.released {
            return Err(Status::unavailable(
                "live guest unavailable; inspect archived output",
            ));
        }
        Ok((
            command.clone(),
            record
                .manifest
                .clone()
                .ok_or_else(|| Status::unavailable("guest manifest missing"))?,
            readers::pin(record)?,
        ))
    }
    async fn live_read(&self, request: LiveOutputRequest) -> Result<LiveOutputObservation, Status> {
        let _permit = self
            .inner
            .output_readers
            .try_acquire()
            .map_err(|_| Status::resource_exhausted("live output readers busy"))?;
        let (scope, read) = live::decode(&request, guardian::wall_ms())?;
        let scoped = scope.clone();
        let (command, client, _reader) = self
            .work(move |h| {
                let (command, manifest, reader) = h.live_record(&scoped)?;
                // Journal guard is dropped before resolving the pinned guest client.
                let client = manifest
                    .guest_client()
                    .map_err(|_| Status::unavailable("guest unavailable"))?;
                Ok((command, client, reader))
            })
            .await?;
        if client.context()
            != command
                .context
                .as_ref()
                .ok_or_else(|| Status::unavailable("guest binding missing"))?
        {
            return Err(Status::failed_precondition("guest binding changed"));
        }
        live::decode(&request, guardian::wall_ms())?;
        let chunk = client
            .output(read.clone())
            .await
            .map_err(|_| Status::unavailable("guest output unavailable"))?;
        let receipt = client
            .inspect(scope.owner.operation_id)
            .await
            .map_err(|_| Status::unavailable("guest receipt unavailable"))?;
        live::validate_chunk(&scope, &read, &command, &chunk, &receipt)?;
        let scoped = scope.clone();
        let current = self
            .work(move |h| h.live_record(&scoped).map(|(c, _, _)| c))
            .await?;
        current
            .validate_receipt(scope.owner.operation_id, &receipt)
            .map_err(uncertain)?;
        live::observation(
            request,
            self.inner.config.host.to_string(),
            self.inner.config.epoch,
            false,
            guardian::wall_ms(),
            chunk,
            receipt,
        )
    }
}
#[tonic::async_trait]
impl LiveOutput for Host {
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
