//! Opt-in history worker, independent of execution and lifecycle maintenance.
use crate::{Controller, ControllerConfig};
use sandbox_protocol::{history::Domain, supervisor::supervisor_client::SupervisorClient};
use sandbox_store::{
    Store,
    history::{Claim, Preparation},
};
use std::time::Duration;
use tonic::transport::Channel;
#[derive(Debug)]
pub struct Retirer {
    store: Store,
    config: ControllerConfig,
    client: SupervisorClient<Channel>,
    next: u8,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistoryTick {
    Idle,
    Completed,
}
#[derive(Debug, thiserror::Error)]
pub enum HistoryError {
    #[error("history retirement storage failed: {0}")]
    Store(#[from] sandbox_store::history::Error),
    #[error("history RPC failed; retained prefix requires reconciliation")]
    Rpc,
}
impl Controller {
    pub fn history_retirer(&self) -> Retirer {
        Retirer {
            store: self.store.clone(),
            config: self.config.clone(),
            client: self.archive_client.clone(),
            next: 0,
        }
    }
}
impl Retirer {
    pub async fn tick(&mut self) -> Result<HistoryTick, HistoryError> {
        let turn = self.next;
        self.next = (self.next + 1) % 4;
        let domain = if turn.is_multiple_of(2) {
            Domain::Commands
        } else {
            Domain::Files
        };
        if turn >= 2 {
            return self.released_tick(domain).await;
        }
        let Some(preparation) = self
            .store
            .claim_history(
                self.config.host,
                self.config.epoch,
                domain,
                120,
                self.config.allow_simulated,
            )
            .await?
        else {
            return Ok(HistoryTick::Idle);
        };
        let claim = match &preparation {
            Preparation::Binding { claim, .. } | Preparation::Retire { claim, .. } => claim.clone(),
        };
        let result = self.process(preparation, &claim).await;
        if result.is_err() {
            let _ = self.store.defer_history(&claim).await;
        }
        result
    }
    async fn released_tick(&mut self, domain: Domain) -> Result<HistoryTick, HistoryError> {
        let Some(prepared) = self
            .store
            .claim_released_history(
                self.config.host,
                self.config.epoch,
                domain,
                120,
                self.config.allow_simulated,
            )
            .await?
        else {
            return Ok(HistoryTick::Idle);
        };
        let result = async {
            let observed = tokio::time::timeout(
                Duration::from_secs(30),
                self.client.retire_released_history(prepared.request),
            )
            .await
            .map_err(|_| HistoryError::Rpc)?
            .map_err(|_| HistoryError::Rpc)?
            .into_inner();
            self.store
                .complete_released_history(&prepared.claim, &observed, self.config.allow_simulated)
                .await?;
            Ok(HistoryTick::Completed)
        }
        .await;
        if result.is_err() {
            let _ = self.store.defer_released_history(&prepared.claim).await;
        }
        result
    }
    async fn process(
        &mut self,
        preparation: Preparation,
        claim: &Claim,
    ) -> Result<HistoryTick, HistoryError> {
        let request = match preparation {
            Preparation::Binding { request, .. } => {
                let observed = tokio::time::timeout(
                    Duration::from_secs(30),
                    self.client.history_binding(request),
                )
                .await
                .map_err(|_| HistoryError::Rpc)?
                .map_err(|_| HistoryError::Rpc)?
                .into_inner();
                self.store
                    .bind_history(claim, &observed, self.config.allow_simulated)
                    .await?
            }
            Preparation::Retire { request, .. } => request,
        };
        let observed =
            tokio::time::timeout(Duration::from_secs(30), self.client.retire_history(request))
                .await
                .map_err(|_| HistoryError::Rpc)?
                .map_err(|_| HistoryError::Rpc)?
                .into_inner();
        self.store
            .complete_history(claim, &observed, self.config.allow_simulated)
            .await?;
        Ok(HistoryTick::Completed)
    }
}
