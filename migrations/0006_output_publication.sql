-- Archiving is independent of execution: a completed command is never made
-- runnable again merely because its output is not yet published.
ALTER TABLE operations
    ADD COLUMN output_status text NOT NULL DEFAULT 'none'
        CHECK (output_status IN ('none', 'pending', 'uploading', 'published', 'expired')),
    ADD COLUMN output_claim_revision bigint NOT NULL DEFAULT 0 CHECK (output_claim_revision >= 0),
    ADD COLUMN output_lease_expires_at timestamptz,
    ADD COLUMN output_next_retry_at timestamptz,
    ADD COLUMN output_ticket jsonb,
    ADD COLUMN output_plan jsonb,
    ADD COLUMN output_expires_at timestamptz,
    ADD CONSTRAINT operations_output_execution CHECK (
        output_status = 'none' OR
        (kind = 'execute' AND execution_allocation_id IS NOT NULL
         AND status IN ('succeeded', 'failed', 'cancelled'))
    ),
    ADD CONSTRAINT operations_output_claim CHECK (
        (output_lease_expires_at IS NULL OR
            (output_claim_revision > 0 AND output_status IN ('pending', 'uploading')))
        AND (output_next_retry_at IS NULL OR output_status IN ('pending', 'uploading'))
    ),
    ADD CONSTRAINT operations_output_metadata CHECK (
        CASE output_status
        WHEN 'none' THEN output_ticket IS NULL AND output_plan IS NULL AND output_expires_at IS NULL AND output_refs = '[]'::jsonb
        WHEN 'pending' THEN output_ticket IS NULL AND output_plan IS NULL AND output_expires_at IS NULL AND output_refs = '[]'::jsonb
        WHEN 'uploading' THEN output_ticket IS NOT NULL AND output_expires_at IS NOT NULL AND output_refs = '[]'::jsonb
        WHEN 'published' THEN output_ticket IS NOT NULL AND output_plan IS NOT NULL AND output_expires_at IS NOT NULL
            AND CASE WHEN jsonb_typeof(output_refs) = 'array' THEN jsonb_array_length(output_refs) = 2 ELSE false END
        WHEN 'expired' THEN output_ticket IS NOT NULL AND output_expires_at IS NOT NULL
        ELSE false END
    );

-- These are candidates only. The store still validates complete ownership,
-- intent, receipt, command digest, statistics and simulation policy before use.
UPDATE operations SET output_status = 'pending'
WHERE kind = 'execute' AND execution_allocation_id IS NOT NULL
  AND status IN ('succeeded', 'failed', 'cancelled') AND attempt_count = 1
  AND jsonb_typeof(attempt_receipts->1->'guest_receipt') = 'object';

CREATE INDEX operations_output_pending_idx ON operations (completed_at, id)
WHERE output_status IN ('pending', 'uploading');
