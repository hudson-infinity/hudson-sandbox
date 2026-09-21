-- Durable inventory only: no object is deleted and no cleanup is declared
-- complete by this migration. Keep execution and deduplication evidence intact.
CREATE TABLE output_cleanup (
    operation_id uuid PRIMARY KEY REFERENCES operations(id),
    claim_revision bigint NOT NULL DEFAULT 0 CHECK (claim_revision >= 0),
    lease_expires_at timestamptz,
    next_retry_at timestamptz,
    manifest jsonb,
    eligible_at timestamptz,
    created_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    CHECK (lease_expires_at IS NULL OR claim_revision > 0),
    CHECK ((manifest IS NULL) = (eligible_at IS NULL)),
    CHECK (manifest IS NULL OR jsonb_typeof(manifest) = 'object')
);

CREATE INDEX output_cleanup_ready_idx ON output_cleanup (next_retry_at, created_at, operation_id);
CREATE INDEX operations_output_expiry_idx ON operations (output_expires_at, id)
WHERE output_status IN ('uploading', 'published', 'expired');
