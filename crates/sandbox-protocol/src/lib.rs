//! Types shared by every component of the service.
//!
//! This crate holds the vocabulary — identities, requests, receipts, errors —
//! and depends on nothing else in the workspace, so the API, controller,
//! supervisor, and guest agent cannot drift in their understanding of it.

pub mod ids;

pub use ids::{HostId, Id, IdParseError, OperationId, ProjectId, SandboxId, SnapshotId};
