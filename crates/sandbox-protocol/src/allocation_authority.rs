//! Bounded allocation authority state machine, not a persistence or release proof.
//!
//! A trusted issuer registers contiguous serials before Create may use them.
//! Retained owners are explicit exceptions below the high-water mark. Forgetting
//! a completed owner therefore cannot make its serial admissible again, and an
//! older live owner does not block reclamation of newer allocations.
//!
//! Integration must durably commit transitions under cross-process launch
//! serialization. Callers independently verify consumer closure, physical cleanup
//! and database acknowledgement before invoking the corresponding transitions.
//! Host retirement uses fencing for deletion. Completion and forgetting remain
//! caller-verified components until the controller handoff is integrated.
use crate::{
    AllocationId, HostId, Id, OperationId, ProjectId, SandboxId, allocation_retirement::Intent,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

pub const MAX_ENTRIES: usize = 1024;
pub const MAX_BATCH: usize = 32;
pub const MAX_BYTES: usize = 1024 * 1024;
pub const MAX_SERIAL: u64 = i64::MAX as u64;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    #[error("invalid authority state or request")]
    Invalid,
    #[error("allocation authority ownership mismatch")]
    Ownership,
    #[error("allocation authority capacity exhausted")]
    Capacity,
    #[error("registration is not the next contiguous batch")]
    RegistrationOrder,
    #[error("serial is closed; this is not release or absence evidence")]
    Closed,
    #[error("serial has not been registered")]
    Unregistered,
    #[error("allocation authority transition conflicts")]
    Conflict,
}

/// Immutable identity issued before host admission. Knowing it grants no access.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Permit {
    pub host: HostId,
    pub project: ProjectId,
    pub sandbox: SandboxId,
    pub allocation: AllocationId,
    pub create_operation: OperationId,
    pub generation: i64,
    pub original_epoch: i64,
    pub serial: u64,
}
impl Permit {
    pub(crate) fn validate(&self) -> Result<(), Error> {
        for id in [
            self.host.uuid(),
            self.project.uuid(),
            self.sandbox.uuid(),
            self.allocation.uuid(),
            self.create_operation.uuid(),
        ] {
            valid_uuid(id)?;
        }
        if self.generation <= 0
            || self.original_epoch <= 0
            || self.serial == 0
            || self.serial > MAX_SERIAL
        {
            return Err(Error::Invalid);
        }
        Ok(())
    }
}
fn valid_uuid(id: uuid::Uuid) -> Result<(), Error> {
    if id.get_version_num() != 7 || id.get_variant() != uuid::Variant::RFC4122 {
        return Err(Error::Invalid);
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum State {
    Active {},
    Fenced {
        retirement: OperationId,
    },
    Complete {
        retirement: OperationId,
        intent_sha256: [u8; 32],
    },
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    permit: Permit,
    state: State,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Saved {
    version: u32,
    host: HostId,
    through: u64,
    entries: Vec<Entry>,
}

/// Transitions are all-or-nothing in memory. Persistence/locking are the caller's
/// responsibility; failure to persist must prevent acknowledgement and launch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Authority(Saved);
impl Authority {
    /// Only for an explicitly new authority. Missing/corrupt storage must never
    /// fall back to this constructor during recovery.
    pub fn new(host: HostId) -> Result<Self, Error> {
        valid_uuid(host.uuid())?;
        Ok(Self(Saved {
            version: 1,
            host,
            through: 0,
            entries: Vec::new(),
        }))
    }
    pub fn through(&self) -> u64 {
        self.0.through
    }
    pub fn retained(&self) -> usize {
        self.0.entries.len()
    }
    pub fn encode(&self) -> Result<Vec<u8>, Error> {
        let bytes = serde_json::to_vec(&self.0).map_err(|_| Error::Invalid)?;
        if bytes.len() > MAX_BYTES {
            return Err(Error::Capacity);
        }
        Ok(bytes)
    }
    /// Validate storage against independently retained host/frontier knowledge.
    /// This cannot detect a self-consistent rollback without an external anchor,
    /// nor does shape validation prove physical cleanup or database completion.
    pub fn decode(bytes: &[u8], host: HostId, minimum_through: u64) -> Result<Self, Error> {
        if bytes.len() > MAX_BYTES {
            return Err(Error::Capacity);
        }
        let saved: Saved = serde_json::from_slice(bytes).map_err(|_| Error::Invalid)?;
        valid_uuid(host.uuid())?;
        if saved.version != 1
            || saved.through > MAX_SERIAL
            || saved.through < minimum_through
            || saved.entries.len() > MAX_ENTRIES
        {
            return Err(Error::Invalid);
        }
        if saved.host != host {
            return Err(Error::Ownership);
        }
        let mut previous = 0;
        let mut allocations = BTreeSet::new();
        let mut operations = BTreeSet::new();
        let mut generations = BTreeSet::new();
        for e in &saved.entries {
            e.permit.validate()?;
            if e.permit.host != host {
                return Err(Error::Ownership);
            }
            if e.permit.serial <= previous
                || e.permit.serial > saved.through
                || !allocations.insert(e.permit.allocation)
                || !operations.insert(e.permit.create_operation)
                || !generations.insert((e.permit.sandbox, e.permit.generation))
            {
                return Err(Error::Invalid);
            }
            previous = e.permit.serial;
            match e.state {
                State::Active {} => {}
                State::Fenced { retirement } | State::Complete { retirement, .. } => {
                    valid_uuid(retirement.uuid())?;
                }
            }
        }
        Ok(Self(saved))
    }
    /// Register the next contiguous batch; Create may then arrive in any order.
    /// An exact wholly retained retry is harmless, but never reactivates fences.
    /// Mixed old/new batches or retries containing forgotten entries fail closed.
    pub fn register(&mut self, permits: &[Permit]) -> Result<(), Error> {
        if permits.is_empty() || permits.len() > MAX_BATCH {
            return Err(Error::Invalid);
        }
        let mut previous = None;
        for p in permits {
            p.validate()?;
            if p.host != self.0.host {
                return Err(Error::Ownership);
            }
            if previous.is_some_and(|n: u64| n.checked_add(1) != Some(p.serial)) {
                return Err(Error::RegistrationOrder);
            }
            previous = Some(p.serial);
        }
        if permits[0].serial <= self.0.through {
            for p in permits {
                self.entry(p)?;
            }
            return Ok(());
        }
        if self.0.through.checked_add(1) != Some(permits[0].serial) {
            return Err(Error::RegistrationOrder);
        }
        if self.0.entries.len() + permits.len() > MAX_ENTRIES {
            return Err(Error::Capacity);
        }
        // Validate the full proposed state before publishing any transition.
        let mut next = self.clone();
        for p in permits {
            next.0.entries.push(Entry {
                permit: p.clone(),
                state: State::Active {},
            });
            next.0.through = p.serial;
        }
        let bytes = next.encode()?;
        Self::decode(&bytes, self.0.host, self.0.through)?;
        *self = next;
        Ok(())
    }
    fn index(&self, permit: &Permit) -> Result<usize, Error> {
        permit.validate()?;
        if permit.host != self.0.host {
            return Err(Error::Ownership);
        }
        match self
            .0
            .entries
            .binary_search_by_key(&permit.serial, |e| e.permit.serial)
        {
            Ok(i) if self.0.entries[i].permit == *permit => Ok(i),
            Ok(_) => Err(Error::Ownership),
            Err(_) if permit.serial <= self.0.through => Err(Error::Closed),
            Err(_) => Err(Error::Unregistered),
        }
    }
    fn entry(&self, permit: &Permit) -> Result<&Entry, Error> {
        Ok(&self.0.entries[self.index(permit)?])
    }
    pub fn state(&self, permit: &Permit) -> Result<&State, Error> {
        Ok(&self.entry(permit)?.state)
    }
    /// Resolve a retained active owner before creating a new host receipt.
    /// Failure is not evidence that an allocation never existed or was released.
    pub fn active_allocation(&self, allocation: AllocationId) -> Result<&Permit, Error> {
        valid_uuid(allocation.uuid())?;
        let entry = self
            .0
            .entries
            .iter()
            .find(|entry| entry.permit.allocation == allocation)
            .ok_or(Error::Ownership)?;
        if entry.state != (State::Active {}) {
            return Err(Error::Conflict);
        }
        Ok(&entry.permit)
    }
    /// All launch/admission entry points must check this under the same durable
    /// authority lock as fencing. A successful check alone is not a launch grant.
    pub fn authorize(&self, permit: &Permit) -> Result<(), Error> {
        match self.state(permit)? {
            State::Active {} => Ok(()),
            _ => Err(Error::Conflict),
        }
    }
    /// Caller has frozen consumer closure in the database. Persist this denial
    /// before observing cleanup or deleting anything. There is no unfence method.
    pub fn fence(&mut self, permit: &Permit, retirement: OperationId) -> Result<(), Error> {
        valid_uuid(retirement.uuid())?;
        let i = self.index(permit)?;
        match self.0.entries[i].state {
            State::Active {} => self.0.entries[i].state = State::Fenced { retirement },
            State::Fenced { retirement: r } | State::Complete { retirement: r, .. }
                if r == retirement => {}
            _ => return Err(Error::Conflict),
        }
        Ok(())
    }
    /// Caller has independently verified exact-owner cleanup and synced metadata
    /// deletion. Bind the full frozen scope before the host record can disappear.
    /// This digest is not a signature or proof of database acknowledgement.
    pub fn complete(&mut self, intent: &Intent) -> Result<(), Error> {
        let complete = completion_state(intent)?;
        let i = self.index(&intent.permit)?;
        match &self.0.entries[i].state {
            State::Fenced { retirement } if *retirement == intent.retirement => {}
            retained if *retained == complete => return Ok(()),
            _ => return Err(Error::Conflict),
        }
        self.0.entries[i].state = complete;
        Ok(())
    }
    /// Read the exact retained completion after a restart or lost response.
    /// Closed after forgetting is only denial, never an exact-scope receipt.
    pub fn completed(&self, intent: &Intent) -> Result<(), Error> {
        let expected = completion_state(intent)?;
        if *self.state(&intent.permit)? != expected {
            return Err(Error::Conflict);
        }
        Ok(())
    }
    /// Caller has verified durable database completion. Afterwards Closed means
    /// only denial, never a receipt for this request's identity or cleanup.
    pub fn forget(&mut self, intent: &Intent) -> Result<(), Error> {
        self.completed(intent)?;
        let i = self.index(&intent.permit)?;
        self.0.entries.remove(i);
        Ok(())
    }
}
fn completion_state(intent: &Intent) -> Result<State, Error> {
    let intent_sha256 = intent.digest().map_err(|_| Error::Invalid)?;
    if intent.simulated {
        return Err(Error::Invalid);
    }
    Ok(State::Complete {
        retirement: intent.retirement,
        intent_sha256,
    })
}

#[cfg(test)]
#[path = "allocation_authority_tests.rs"]
mod tests;
