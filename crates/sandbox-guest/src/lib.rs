//! Guest-local command execution. Host watchdog and VM isolation remain separate requirements.
#[cfg(target_os = "linux")]
pub mod launcher;
pub mod model;
#[cfg(target_os = "linux")]
pub mod runner;
