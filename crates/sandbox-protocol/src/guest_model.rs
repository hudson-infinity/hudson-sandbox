//! Bounded guest command input and receipts. No argv/environment values enter receipts or Debug.
use crate::{AllocationId, OperationId};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, fmt};

pub const MAX_OUTPUT: u64 = 16 * 1024 * 1024;
pub const MAX_RECORDS: usize = 128;
pub const MAX_RESERVED_OUTPUT: u64 = 64 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Context {
    pub allocation_id: AllocationId,
    pub generation: i64,
    pub boot_id: String,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Execute {
    pub operation_id: OperationId,
    pub argv: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub cwd: String,
    pub deadline_unix_ms: i64,
    /// Combined retained stdout/stderr bytes. Capture continues draining after this limit.
    pub output_limit: u64,
}
impl fmt::Debug for Execute {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Execute")
            .field("operation_id", &self.operation_id)
            .finish_non_exhaustive()
    }
}
impl Execute {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            !self.argv.is_empty() && self.argv.len() <= 256,
            "invalid argv count"
        );
        ensure!(!self.argv[0].is_empty(), "empty executable");
        ensure!(
            self.argv.iter().all(|v| !v.contains('\0'))
                && self.argv.iter().map(String::len).sum::<usize>() <= 32768,
            "invalid argv"
        );
        ensure!(
            self.env.len() <= 128
                && self
                    .env
                    .iter()
                    .map(|(k, v)| k.len() + v.len())
                    .sum::<usize>()
                    <= 16384,
            "environment too large"
        );
        for (key, value) in &self.env {
            ensure!(
                !key.is_empty()
                    && key.bytes().enumerate().all(|(i, b)| b == b'_'
                        || b.is_ascii_alphabetic()
                        || (i > 0 && b.is_ascii_digit()))
                    && !value.contains('\0'),
                "invalid environment"
            );
        }
        ensure!(
            self.cwd.starts_with('/') && self.cwd.len() <= 4096 && !self.cwd.contains('\0'),
            "invalid working directory"
        );
        ensure!(
            (1..=MAX_OUTPUT).contains(&self.output_limit),
            "invalid output limit"
        );
        ensure!(
            serde_json::to_vec(self)?.len() <= 65536,
            "encoded request too large"
        );
        Ok(())
    }
    pub fn digest(&self) -> Result<[u8; 32]> {
        let mut hash = Sha256::new();
        hash.update(b"hudson-guest-execute-v1\0");
        hash.update(serde_json::to_vec(self)?);
        Ok(hash.finalize().into())
    }
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum State {
    LaunchIntent,
    Exited,
    TimedOut,
    Cancelled,
    Unknown,
}
impl State {
    pub fn terminal(self) -> bool {
        self != Self::LaunchIntent
    }
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Output {
    pub seen: u64,
    pub stored: u64,
    pub truncated: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Receipt {
    pub version: u32,
    pub context: Context,
    pub operation_id: OperationId,
    pub digest: [u8; 32],
    pub state: State,
    pub deadline_unix_ms: i64,
    pub output_limit: u64,
    pub cancel_requested: bool,
    pub cleanup_confirmed: bool,
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    pub stdout: Output,
    pub stderr: Output,
    pub reason: Option<String>,
}

impl Receipt {
    /// Shape validation only; guest data cannot prove host cleanup or trusted command outcomes.
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.version == 1
                && self.context.generation > 0
                && !self.context.boot_id.is_empty()
                && self.context.boot_id.len() <= 64,
            "invalid receipt context"
        );
        ensure!(
            (1..=MAX_OUTPUT).contains(&self.output_limit)
                && self.stdout.stored <= self.stdout.seen
                && self.stderr.stored <= self.stderr.seen
                && self
                    .stdout
                    .stored
                    .checked_add(self.stderr.stored)
                    .is_some_and(|n| n <= self.output_limit)
                && self.stdout.truncated == (self.stdout.stored < self.stdout.seen)
                && self.stderr.truncated == (self.stderr.stored < self.stderr.seen)
                && self.reason.as_ref().is_none_or(|s| s.len() <= 256),
            "invalid receipt output or reason"
        );
        if self.state == State::Exited {
            ensure!(
                self.exit_code.is_some() ^ self.signal.is_some(),
                "missing or ambiguous exit status"
            );
            ensure!(
                self.exit_code.is_none_or(|n| (0..=255).contains(&n))
                    && self.signal.is_none_or(|n| (1..=64).contains(&n)),
                "invalid exit status"
            );
        } else {
            ensure!(
                self.exit_code.is_none() && self.signal.is_none(),
                "unexpected exit status"
            );
        }
        ensure!(
            self.state != State::LaunchIntent || !self.cleanup_confirmed,
            "active receipt claims cleanup"
        );
        ensure!(
            !matches!(
                self.state,
                State::Exited | State::TimedOut | State::Cancelled
            ) || self.cleanup_confirmed,
            "terminal receipt lacks cleanup"
        );
        ensure!(
            self.state != State::Cancelled || self.cancel_requested,
            "cancelled without request"
        );
        Ok(())
    }
}
