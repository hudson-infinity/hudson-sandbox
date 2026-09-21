//! One configured host, durable create/destroy dispatch, and reconciliation over mTLS.
//! The supervisor is trusted only after its certificate, host ID, and epoch match.

use sandbox_protocol::{
    HostId,
    supervisor::{
        AllocationState, HealthRequest, InspectRequest, StopRequest,
        supervisor_client::SupervisorClient,
    },
};
use sandbox_store::{
    Store,
    claims::{Claim, ClaimError, OperationKind},
    destroy::DestroyAction,
    dispatch::{CreateAction, CreateRejection, DispatchError},
    placement::{PlacementError, Reservation},
};
use sandbox_supervisor::transport::{self, TransportError};
use std::collections::BTreeSet;
use tonic::transport::Channel;

#[derive(Debug, Clone)]
pub struct ControllerConfig {
    pub endpoint: String,
    pub host: HostId,
    pub epoch: i64,
    pub allowed_images: BTreeSet<String>,
    pub allow_simulated: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum ControllerError {
    #[error("configure a positive host epoch and nonempty immutable image allowlist")]
    InvalidConfig,
    #[error("supervisor health response does not match the configured host or evidence policy")]
    HostIdentity,
    #[error("supervisor health request failed")]
    Health,
    #[error(transparent)]
    Transport(#[from] TransportError),
    #[error(transparent)]
    Claim(#[from] ClaimError),
    #[error(transparent)]
    Placement(#[from] PlacementError),
    #[error(transparent)]
    Dispatch(#[from] DispatchError),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tick {
    Idle,
    Deferred,
    Rejected,
    Confirmed,
    Unknown,
    LostOwnership,
}

#[derive(Debug)]
pub struct Controller {
    store: Store,
    config: ControllerConfig,
    client: SupervisorClient<Channel>,
}

impl Controller {
    pub async fn connect(
        store: Store,
        config: ControllerConfig,
        ca: &[u8],
        cert: &[u8],
        key: &[u8],
    ) -> Result<Self, ControllerError> {
        if config.epoch <= 0
            || config.allowed_images.is_empty()
            || config
                .allowed_images
                .iter()
                .any(|digest| !sandbox_protocol::images::valid_image_digest(digest))
        {
            return Err(ControllerError::InvalidConfig);
        }
        let client = transport::connect(&config.endpoint, config.host, ca, cert, key).await?;
        let mut controller = Self {
            store,
            config,
            client,
        };
        controller.check_host().await?;
        Ok(controller)
    }

    async fn check_host(&mut self) -> Result<(), ControllerError> {
        let health = self
            .client
            .health(HealthRequest {})
            .await
            .map_err(|_| ControllerError::Health)?
            .into_inner();
        if health.host_id != self.config.host.to_string()
            || health.supervisor_epoch != self.config.epoch
            || (health.simulated && !self.config.allow_simulated)
        {
            return Err(ControllerError::HostIdentity);
        }
        // This only touches the existing operator-provisioned epoch. Registration
        // and epoch issuance remain separate; the RPC cannot insert arbitrary hosts.
        self.store
            .observe_configured_host(self.config.host, self.config.epoch)
            .await?;
        Ok(())
    }

    async fn defer(&self, claim: &Claim) -> Result<Tick, ControllerError> {
        match self.store.defer_claim(claim, 5).await {
            Ok(()) => Ok(Tick::Deferred),
            Err(ClaimError::LostClaim) => Ok(Tick::LostOwnership),
            Err(error) => Err(error.into()),
        }
    }

    async fn reject(
        &self,
        claim: &Claim,
        reason: CreateRejection,
    ) -> Result<Tick, ControllerError> {
        match self.store.reject_undispatched_create(claim, reason).await {
            Ok(()) => Ok(Tick::Rejected),
            Err(DispatchError::Conflict) => self.defer(claim).await,
            Err(DispatchError::LostClaim) => Ok(Tick::LostOwnership),
            Err(error) => Err(error.into()),
        }
    }

    async fn unknown(&self, claim: &Claim) -> Result<Tick, ControllerError> {
        match self.store.record_create_unknown(claim).await {
            Ok(()) => Ok(Tick::Unknown),
            Err(DispatchError::LostClaim) => Ok(Tick::LostOwnership),
            Err(DispatchError::Conflict) => self.defer(claim).await,
            Err(error) => Err(error.into()),
        }
    }

    async fn destroy_unknown(&self, claim: &Claim) -> Result<Tick, ControllerError> {
        match self.store.record_destroy_unknown(claim).await {
            Ok(()) => Ok(Tick::Unknown),
            Err(DispatchError::LostClaim) => Ok(Tick::LostOwnership),
            Err(DispatchError::Conflict) => self.defer(claim).await,
            Err(error) => Err(error.into()),
        }
    }

    async fn destroy_tick(&mut self, claim: &Claim) -> Result<Tick, ControllerError> {
        let action = match self.store.prepare_destroy(claim, false).await {
            Ok(action) => action,
            Err(DispatchError::LostClaim) => return Ok(Tick::LostOwnership),
            Err(DispatchError::Conflict) => return self.defer(claim).await,
            Err(error) => return Err(error.into()),
        };
        let owner = match &action {
            DestroyAction::Stop(owner) | DestroyAction::Inspect(owner) => owner,
        };
        if owner.host_id != self.config.host.to_string()
            || owner.supervisor_epoch != self.config.epoch
        {
            return self.defer(claim).await;
        }
        let response = match action {
            DestroyAction::Stop(owner) => {
                self.client
                    .stop(StopRequest {
                        ownership: Some(owner),
                    })
                    .await
            }
            DestroyAction::Inspect(owner) => {
                match self
                    .client
                    .inspect(InspectRequest {
                        ownership: Some(owner.clone()),
                    })
                    .await
                {
                    Ok(response)
                        if response.get_ref().ownership.as_ref() == Some(&owner)
                            && matches!(
                                AllocationState::try_from(response.get_ref().state),
                                Ok(AllocationState::Ready | AllocationState::Absent)
                            ) =>
                    {
                        // Stop is an idempotent fence for this exact incarnation,
                        // but each transport attempt still commits intent first.
                        let next = match self.store.prepare_destroy(claim, true).await {
                            Ok(next) => next,
                            Err(DispatchError::LostClaim) => return Ok(Tick::LostOwnership),
                            Err(DispatchError::Conflict) => return self.defer(claim).await,
                            Err(error) => return Err(error.into()),
                        };
                        let DestroyAction::Stop(owner) = next else {
                            return self.destroy_unknown(claim).await;
                        };
                        self.client
                            .stop(StopRequest {
                                ownership: Some(owner),
                            })
                            .await
                    }
                    other => other,
                }
            }
        };
        let observation = match response {
            Ok(response) => response.into_inner(),
            Err(_) => return self.destroy_unknown(claim).await,
        };
        match self
            .store
            .record_destroy_observation(claim, &observation, self.config.allow_simulated)
            .await
        {
            Ok(()) => Ok(Tick::Confirmed),
            Err(DispatchError::LostClaim) => Ok(Tick::LostOwnership),
            Err(DispatchError::BadEvidence | DispatchError::SimulationDenied) => {
                self.destroy_unknown(claim).await
            }
            Err(DispatchError::Conflict) => self.defer(claim).await,
            Err(error) => Err(error.into()),
        }
    }

    /// At most one claimed operation per tick. Cancellation after durable intent
    /// leaves work reclaimable; the next owner inspects instead of replaying it.
    pub async fn tick(&mut self) -> Result<Tick, ControllerError> {
        self.check_host().await?;
        if let Some(claim) = self.store.claim_next(OperationKind::Destroy, 30).await? {
            return self.destroy_tick(&claim).await;
        }
        let Some(claim) = self.store.claim_next(OperationKind::Create, 30).await? else {
            return Ok(Tick::Idle);
        };
        let reservation = match self
            .store
            .reserve_create(&claim, self.config.host, self.config.epoch)
            .await
        {
            Ok(value) => value,
            Err(PlacementError::Unauthorized) => {
                return self.reject(&claim, CreateRejection::Unauthorized).await;
            }
            Err(PlacementError::InvalidResources) => {
                return self.reject(&claim, CreateRejection::InvalidResources).await;
            }
            Err(
                PlacementError::Capacity
                | PlacementError::Quota
                | PlacementError::HostUnavailable
                | PlacementError::Reconcile,
            ) => return self.defer(&claim).await,
            Err(PlacementError::LostClaim) => return Ok(Tick::LostOwnership),
            Err(error) => return Err(error.into()),
        };
        let allocation = match reservation {
            Reservation::Reserved(a) | Reservation::Existing(a) => a,
        };
        if allocation.host_id != self.config.host
            || allocation.supervisor_epoch != self.config.epoch
        {
            return self.defer(&claim).await;
        }
        let action = match self
            .store
            .prepare_create_dispatch(&claim, &self.config.allowed_images)
            .await
        {
            Ok(action) => action,
            Err(DispatchError::Unauthorized) => {
                return self.reject(&claim, CreateRejection::Unauthorized).await;
            }
            Err(DispatchError::ImageDenied) => {
                return self.reject(&claim, CreateRejection::ImageDenied).await;
            }
            Err(DispatchError::HostUnavailable | DispatchError::Conflict) => {
                return self.defer(&claim).await;
            }
            Err(DispatchError::LostClaim) => return Ok(Tick::LostOwnership),
            Err(error) => return Err(error.into()),
        };
        let response = match action {
            CreateAction::Start(request) => self.client.create(request).await,
            CreateAction::Inspect(owner) => {
                self.client
                    .inspect(InspectRequest {
                        ownership: Some(owner),
                    })
                    .await
            }
        };
        let observation = match response {
            Ok(response) => response.into_inner(),
            Err(_) => return self.unknown(&claim).await,
        };
        match self
            .store
            .record_create_observation(&claim, &observation, self.config.allow_simulated)
            .await
        {
            Ok(()) => Ok(Tick::Confirmed),
            Err(DispatchError::BadEvidence | DispatchError::SimulationDenied) => {
                self.unknown(&claim).await
            }
            Err(DispatchError::LostClaim) => Ok(Tick::LostOwnership),
            Err(DispatchError::Conflict) => self.defer(&claim).await,
            Err(error) => Err(error.into()),
        }
    }
}
