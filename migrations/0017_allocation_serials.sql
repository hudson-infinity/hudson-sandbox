-- Transactional issuance, not a PostgreSQL sequence: rollback consumes no serial.
-- Legacy allocations deliberately remain unissued until exact-owner migration
-- is implemented. This migration grants no host/guardian deletion authority.
ALTER TABLE hosts ADD COLUMN last_allocation_serial bigint NOT NULL DEFAULT 0
    CHECK (last_allocation_serial >= 0);

CREATE TABLE allocation_permits (
    allocation_id uuid PRIMARY KEY REFERENCES allocations(id),
    host_id uuid NOT NULL REFERENCES hosts(id),
    serial bigint NOT NULL CHECK (serial > 0),
    project_id uuid NOT NULL REFERENCES projects(id),
    sandbox_id uuid NOT NULL,
    create_operation_id uuid NOT NULL UNIQUE,
    generation bigint NOT NULL CHECK (generation > 0),
    original_epoch bigint NOT NULL CHECK (original_epoch > 0),
    UNIQUE (host_id, serial),
    UNIQUE (sandbox_id, generation),
    FOREIGN KEY (project_id, sandbox_id) REFERENCES sandboxes(project_id, id),
    FOREIGN KEY (sandbox_id, allocation_id) REFERENCES allocations(sandbox_id, id),
    FOREIGN KEY (sandbox_id, create_operation_id) REFERENCES operations(sandbox_id, id)
);
