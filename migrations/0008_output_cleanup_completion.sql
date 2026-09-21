-- Completion records verified retirement of one frozen upload attempt. It is
-- independent of execution success, VM teardown and journal compaction.
ALTER TABLE output_cleanup
    ADD COLUMN completed_at timestamptz,
    ADD COLUMN receipt jsonb,
    ADD CONSTRAINT output_cleanup_completion CHECK (
        (completed_at IS NULL AND receipt IS NULL) OR
        (completed_at IS NOT NULL AND receipt IS NOT NULL
         AND jsonb_typeof(receipt) = 'object' AND manifest IS NOT NULL
         AND claim_revision > 0 AND completed_at >= eligible_at
         AND lease_expires_at IS NULL AND next_retry_at IS NULL)
    );

DROP INDEX output_cleanup_ready_idx;
CREATE INDEX output_cleanup_ready_idx ON output_cleanup (created_at, operation_id)
WHERE completed_at IS NULL;
