-- Separate storage retirement from operation outcomes and guest reservations.
ALTER TABLE file_uploads
    ADD COLUMN source_frozen_at timestamptz,
    ADD COLUMN source_cleanup_manifest jsonb,
    ADD COLUMN source_cleanup_revision bigint NOT NULL DEFAULT 0 CHECK (source_cleanup_revision>=0),
    ADD COLUMN source_cleanup_lease_until timestamptz,
    ADD COLUMN source_cleanup_next_at timestamptz,
    ADD COLUMN source_retired_at timestamptz,
    ADD COLUMN source_retirement jsonb,
    ADD CONSTRAINT file_source_frozen_manifest CHECK (
        (source_frozen_at IS NULL AND source_cleanup_manifest IS NULL AND source_cleanup_revision=0
         AND source_cleanup_lease_until IS NULL AND source_cleanup_next_at IS NULL)
        OR (source_frozen_at IS NOT NULL AND source_cleanup_manifest IS NOT NULL
            AND jsonb_typeof(source_cleanup_manifest)='object' AND source_cleanup_revision>0)),
    ADD CONSTRAINT file_source_retirement_paired CHECK (
        (source_retired_at IS NULL AND source_retirement IS NULL)
        OR (source_retired_at IS NOT NULL AND source_retirement IS NOT NULL
            AND jsonb_typeof(source_retirement)='object' AND source_frozen_at IS NOT NULL
            AND source_retired_at>=source_frozen_at AND source_cleanup_lease_until IS NULL));
CREATE INDEX file_source_cleanup_pending ON file_uploads(source_cleanup_next_at,operation_id)
WHERE source_retired_at IS NULL;
