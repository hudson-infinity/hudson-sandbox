-- Independent registration checkpoint; no retirement or release authority.
ALTER TABLE hosts ADD COLUMN launch_authority_required boolean NOT NULL DEFAULT false;
ALTER TABLE hosts ADD COLUMN registered_allocation_serial bigint NOT NULL DEFAULT 0
    CHECK (registered_allocation_serial >= 0 AND registered_allocation_serial <= last_allocation_serial);
