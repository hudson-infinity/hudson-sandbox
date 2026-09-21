//! Shared authenticated supervisor transport. VM execution remains unimplemented.
//! The fake host uses this boundary on macOS; the real supervisor will use it on Linux.

pub mod transport;

#[cfg(unix)]
pub mod guest;

#[cfg(target_os = "linux")]
pub mod guardian;
pub mod lease;
