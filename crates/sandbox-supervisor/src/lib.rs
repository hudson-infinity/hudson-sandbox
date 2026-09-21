//! Authenticated supervisor transport, Linux guardian and durable lifecycle service.

pub mod archive;
pub mod transport;

#[cfg(unix)]
pub mod guest;

#[cfg(target_os = "linux")]
pub mod guardian;
pub mod lease;

pub mod identity;

#[cfg(target_os = "linux")]
pub mod host;

pub mod live_output;

pub mod file_downloads;
