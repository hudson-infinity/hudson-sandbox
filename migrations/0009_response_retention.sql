-- Explicit operator policy assigns deadlines later. Upgrading alone neither
-- expires existing responses nor removes any execution/deduplication evidence.
CREATE INDEX operations_response_retention_idx ON operations (completed_at, id)
WHERE status IN ('succeeded', 'failed', 'cancelled')
AND completed_at IS NOT NULL AND response_expires_at IS NULL;
