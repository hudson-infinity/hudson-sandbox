//! Operator-owned response retention. Expiry hides a terminal response; it
//! never authorizes deletion of retry keys, payloads or reconciliation evidence.
use crate::{Store, StoreError};

#[derive(Debug, Clone, Copy)]
pub struct ResponseRetention(u32);
impl ResponseRetention {
    /// Explicit policy between one second and 365 days. No implicit default.
    pub fn new(seconds: u32) -> Option<Self> {
        (1..=31_536_000).contains(&seconds).then_some(Self(seconds))
    }
}

impl Store {
    /// Assign at most 100 previously unset deadlines, measured from confirmed
    /// completion rather than discovery. Concurrent workers skip locked rows;
    /// retries and later configuration changes never extend existing deadlines.
    pub async fn assign_response_retention(
        &self,
        policy: ResponseRetention,
    ) -> Result<u64, StoreError> {
        sqlx::query(
            "WITH candidates AS (
                SELECT id FROM operations
                WHERE status IN ('succeeded','failed','cancelled')
                AND completed_at IS NOT NULL AND response_expires_at IS NULL
                ORDER BY completed_at,id FOR UPDATE SKIP LOCKED LIMIT 100
            )
            UPDATE operations o SET response_expires_at=o.completed_at+make_interval(secs=>$1)
            FROM candidates c WHERE o.id=c.id AND o.response_expires_at IS NULL
            AND o.status IN ('succeeded','failed','cancelled') AND o.completed_at IS NOT NULL",
        )
        .bind(f64::from(policy.0))
        .execute(self.pool())
        .await
        .map(|r| r.rows_affected())
        .map_err(StoreError::Query)
    }
}
