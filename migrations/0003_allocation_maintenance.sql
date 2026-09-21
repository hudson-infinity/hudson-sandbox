-- Maintenance owns an allocation independently of completed create operations.
-- Existing running allocations become eligible immediately; no lease is extended
-- and no historical observation is invented by this upgrade.
ALTER TABLE allocations
    ADD COLUMN maintenance_revision bigint NOT NULL DEFAULT 0 CHECK (maintenance_revision >= 0),
    ADD COLUMN maintenance_lease_until timestamptz,
    ADD COLUMN maintenance_next_at timestamptz,
    ADD COLUMN renewal_pending boolean NOT NULL DEFAULT false,
    ADD COLUMN lease_requested_until timestamptz,
    ADD COLUMN lease_observation jsonb;

CREATE INDEX allocations_maintenance_idx ON allocations (host_id, maintenance_next_at)
    WHERE released_at IS NULL AND status = 'running';
