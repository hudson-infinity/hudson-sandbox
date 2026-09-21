//! Types shared by every component of the service.
//!
//! This crate holds the vocabulary — identities, requests, receipts, errors —
//! and depends on nothing else in the workspace, so the API, controller,
//! supervisor, and guest agent cannot drift in their understanding of it.

pub mod idempotency;
pub mod ids;
pub mod images;
pub mod token;

pub use idempotency::{DIGEST_VERSION, IdempotencyKey, KeyError, RequestDigest};
pub use ids::{
    AllocationId, HostId, Id, IdParseError, OperationId, ProjectId, SandboxId, SnapshotId,
};
pub use token::{ProjectToken, TokenHash, TokenKeyId, TokenParseError};

/// Versioned internal supervisor messages and generated gRPC client/server.
pub mod supervisor {
    tonic::include_proto!("hudson.supervisor.v1");
}

/// Versioned guest command wire messages. Treat all guest reports as untrusted.
pub mod guest {
    tonic::include_proto!("hudson.guest.v1");
}
pub mod guest_model;
pub mod guest_wire;

pub mod bootstrap;
