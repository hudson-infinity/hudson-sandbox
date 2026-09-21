//! The in-guest agent: spawns customer processes, captures bounded output,
//! transfers files, and holds the workload frozen across a pause.
//!
//! Not implemented. This crate only builds for Linux targets, because it
//! needs cgroups and vsock. See `docs/implementation/dev-env.md` for how to
//! work on it from macOS.
