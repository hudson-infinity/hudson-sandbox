//! Pins cover host reads from admission through their final ownership check.
use super::*;
pub(super) type Pin = tokio::sync::OwnedRwLockReadGuard<()>;

/// Caller holds the journal mutex so closure and pin admission are atomic.
pub(super) fn pin(record: &Record) -> Result<Pin, Status> {
    retirement::check(record)?;
    if record.stopped || record.released {
        return Err(Status::unavailable("allocation stopped"));
    }
    record
        .readers
        .clone()
        .try_read_owned()
        .map_err(|_| Status::unavailable("allocation readers closed"))
}
