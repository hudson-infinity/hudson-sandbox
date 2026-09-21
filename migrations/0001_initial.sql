-- Initial schema: the five records the first phase's operations touch.
--
-- Specified by docs/data-models.md. Snapshots arrive with pause/resume in
-- phase 3; UI sessions and the audit trail arrive with the management UI in
-- phase 4. Deferring a table is not deferring correctness — the ownership
-- columns that make recovery possible (generation, supervisor_epoch,
-- claim_revision) are here from the start, because retrofitting them into a
-- working system is what this order exists to avoid.
--
-- States are text with CHECK constraints rather than PostgreSQL enums.
-- Widening a CHECK is an ordinary migration; adding an enum value is not
-- transactional in every supported version, and the set is still moving.
--
-- The application sets updated_at. No trigger: a trigger that silently
-- rewrites a row is exactly the kind of invisible state change the lifecycle
-- contract is trying to avoid.

CREATE TABLE projects (
    id                  uuid PRIMARY KEY,
    name                text        NOT NULL,
    status              text        NOT NULL
        CHECK (status IN ('active', 'suspended', 'deleting')),
    -- CPU, memory, disk, snapshot storage, sandbox count, pending operations.
    limits              jsonb       NOT NULL,
    -- Bounded token metadata: key id, SHA-256 hash, created/expires/revoked.
    -- Never a raw credential. At most two active, so rotation is possible.
    api_tokens          jsonb       NOT NULL DEFAULT '[]'::jsonb,
    external_reference  text,
    created_at          timestamptz NOT NULL DEFAULT now(),
    updated_at          timestamptz NOT NULL DEFAULT now(),

    CONSTRAINT projects_api_tokens_is_array CHECK (jsonb_typeof(api_tokens) = 'array'),
    CONSTRAINT projects_limits_is_object    CHECK (jsonb_typeof(limits) = 'object')
);

CREATE TABLE hosts (
    id                    uuid PRIMARY KEY,
    status                text        NOT NULL
        CHECK (status IN ('ready', 'draining', 'unhealthy')),
    last_seen_at          timestamptz,
    -- Schedulable capacity, already net of OS and image-cache headroom.
    cpu_capacity          integer     NOT NULL CHECK (cpu_capacity > 0),
    memory_capacity_mib   bigint      NOT NULL CHECK (memory_capacity_mib > 0),
    disk_capacity_mib     bigint      NOT NULL CHECK (disk_capacity_mib > 0),
    compatibility         jsonb       NOT NULL DEFAULT '{}'::jsonb,
    max_snapshot_uploads  integer     NOT NULL DEFAULT 1 CHECK (max_snapshot_uploads >= 0),
    -- Increases on every registration. A restarted supervisor gets a new
    -- epoch, so messages from the previous incarnation are rejected. Distinct
    -- from an OS boot id, and requires reconciliation before ownership is
    -- renewed.
    supervisor_epoch      bigint      NOT NULL DEFAULT 0 CHECK (supervisor_epoch >= 0),
    created_at            timestamptz NOT NULL DEFAULT now(),
    updated_at            timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE sandboxes (
    id                    uuid PRIMARY KEY,
    project_id            uuid        NOT NULL REFERENCES projects (id),
    name                  text,
    labels                jsonb       NOT NULL DEFAULT '{}'::jsonb,
    image_digest          text        NOT NULL,
    image_compatibility   jsonb       NOT NULL DEFAULT '{}'::jsonb,
    resources             jsonb       NOT NULL,
    -- Intent and observation are separate columns on purpose. A desired state
    -- is not evidence that a VM reached it.
    desired_state         text        NOT NULL
        CHECK (desired_state IN ('running', 'paused', 'destroyed')),
    observed_state        text        NOT NULL
        CHECK (observed_state IN ('creating', 'running', 'pausing', 'paused',
                                  'resuming', 'destroying', 'destroyed', 'unknown')),
    observed_at           timestamptz,
    state_revision        bigint      NOT NULL DEFAULT 0 CHECK (state_revision >= 0),
    -- Orders allocation replacements and rejects commands from an old VM
    -- incarnation. A failed allocation consumes its generation.
    generation            bigint      NOT NULL DEFAULT 0 CHECK (generation >= 0),
    current_allocation_id uuid,
    -- Serializes create, pause, resume and destroy against each other.
    active_transition_operation_id uuid,
    expires_at            timestamptz,
    destroyed_at          timestamptz,
    created_at            timestamptz NOT NULL DEFAULT now(),
    updated_at            timestamptz NOT NULL DEFAULT now(),

    -- Referenced by composite foreign keys below, so a sandbox-local link
    -- cannot point at another project's row.
    CONSTRAINT sandboxes_project_id_id_key UNIQUE (project_id, id),
    CONSTRAINT sandboxes_resources_is_object CHECK (jsonb_typeof(resources) = 'object'),
    CONSTRAINT sandboxes_destroyed_state CHECK (
        (destroyed_at IS NULL) OR (observed_state = 'destroyed' OR desired_state = 'destroyed')
    )
);

CREATE INDEX sandboxes_project_id_idx ON sandboxes (project_id);

CREATE TABLE operations (
    id                  uuid PRIMARY KEY,
    project_id          uuid        NOT NULL REFERENCES projects (id),
    sandbox_id          uuid        NOT NULL,
    kind                text        NOT NULL
        CHECK (kind IN ('create', 'execute', 'destroy', 'cancel', 'file_write')),
    -- Server-assigned. A client payload can never set these, and a null key
    -- id never implies service authority.
    initiator_kind      text        NOT NULL
        CHECK (initiator_kind IN ('project', 'admin', 'service')),
    initiator_key_id    text,
    initiator_session_id uuid,
    -- The caller's key for one logical mutation, and a digest of the
    -- normalized request so a retry carrying different content conflicts
    -- instead of silently running something else.
    idempotency_key     text        NOT NULL
        CHECK (length(idempotency_key) BETWEEN 16 AND 128),
    request_digest      bytea       NOT NULL,
    digest_version      integer     NOT NULL CHECK (digest_version >= 1),
    payload             jsonb       NOT NULL,
    input_refs          jsonb       NOT NULL DEFAULT '{}'::jsonb,
    target_operation_id uuid,
    status              text        NOT NULL
        CHECK (status IN ('queued', 'running', 'succeeded', 'failed', 'cancelled', 'unknown')),
    phase               text,
    result              jsonb,
    error               jsonb,
    output_refs         jsonb       NOT NULL DEFAULT '[]'::jsonb,
    attempt_count       integer     NOT NULL DEFAULT 0 CHECK (attempt_count >= 0),
    -- Dispatch, acknowledgement and completion evidence, each bound to the
    -- allocation and claim revision it happened under. Earlier entries are
    -- preserved rather than rewritten.
    attempt_receipts    jsonb       NOT NULL DEFAULT '[]'::jsonb,
    receipt_history_ref text,
    -- Orders controller ownership even when the allocation has not changed.
    claim_revision      bigint      NOT NULL DEFAULT 0 CHECK (claim_revision >= 0),
    lease_expires_at    timestamptz,
    next_retry_at       timestamptz,
    deadline            timestamptz,
    response_expires_at timestamptz,
    completed_at        timestamptz,
    created_at          timestamptz NOT NULL DEFAULT now(),
    updated_at          timestamptz NOT NULL DEFAULT now(),

    -- One logical mutation per key, per project. This is what makes a retry
    -- safe, and the tombstone that outlives the payload keeps it safe
    -- afterwards.
    CONSTRAINT operations_project_idempotency_key UNIQUE (project_id, idempotency_key),
    CONSTRAINT operations_sandbox_id_id_key UNIQUE (sandbox_id, id),
    CONSTRAINT operations_sandbox_in_same_project
        FOREIGN KEY (project_id, sandbox_id) REFERENCES sandboxes (project_id, id),
    CONSTRAINT operations_cancel_target_same_sandbox
        FOREIGN KEY (sandbox_id, target_operation_id) REFERENCES operations (sandbox_id, id),
    CONSTRAINT operations_cancel_has_target CHECK (
        (kind = 'cancel') = (target_operation_id IS NOT NULL)
    ),
    CONSTRAINT operations_terminal_has_completed_at CHECK (
        (status IN ('succeeded', 'failed', 'cancelled')) = (completed_at IS NOT NULL)
    )
);

-- The controller's claim query: unfinished work, oldest first.
CREATE INDEX operations_pending_idx
    ON operations (created_at)
    WHERE status IN ('queued', 'running');

CREATE INDEX operations_sandbox_id_idx ON operations (sandbox_id);

CREATE TABLE allocations (
    id                uuid PRIMARY KEY,
    project_id        uuid        NOT NULL REFERENCES projects (id),
    sandbox_id        uuid        NOT NULL,
    host_id           uuid        NOT NULL REFERENCES hosts (id),
    generation        bigint      NOT NULL CHECK (generation > 0),
    supervisor_epoch  bigint      NOT NULL CHECK (supervisor_epoch >= 0),
    vcpu              integer     NOT NULL CHECK (vcpu > 0),
    memory_mib        bigint      NOT NULL CHECK (memory_mib > 0),
    disk_mib          bigint      NOT NULL CHECK (disk_mib > 0),
    status            text        NOT NULL
        CHECK (status IN ('reserved', 'running', 'releasing', 'released')),
    lease_expires_at  timestamptz,
    -- Confirmed termination or fencing. Without it the reservation still
    -- counts against capacity, whatever the desired state says.
    release_evidence  jsonb,
    released_at       timestamptz,
    created_at        timestamptz NOT NULL DEFAULT now(),
    updated_at        timestamptz NOT NULL DEFAULT now(),

    CONSTRAINT allocations_sandbox_generation_key UNIQUE (sandbox_id, generation),
    CONSTRAINT allocations_sandbox_id_id_key UNIQUE (sandbox_id, id),
    CONSTRAINT allocations_sandbox_in_same_project
        FOREIGN KEY (project_id, sandbox_id) REFERENCES sandboxes (project_id, id),
    CONSTRAINT allocations_released_has_evidence CHECK (
        (released_at IS NULL) = (release_evidence IS NULL)
    ),
    CONSTRAINT allocations_released_status CHECK (
        (status = 'released') = (released_at IS NOT NULL)
    )
);

-- At most one live allocation per sandbox. Two VMs for one sandbox is the
-- failure this constraint exists to make impossible.
CREATE UNIQUE INDEX allocations_one_unreleased_per_sandbox
    ON allocations (sandbox_id)
    WHERE released_at IS NULL;

-- Capacity is computed from unreleased allocations, so that query has an index.
CREATE INDEX allocations_host_unreleased_idx
    ON allocations (host_id)
    WHERE released_at IS NULL;

-- Sandbox pointers close the loop, now that both targets exist. Composite so
-- a sandbox cannot point at another sandbox's allocation or operation.
ALTER TABLE sandboxes
    ADD CONSTRAINT sandboxes_current_allocation_same_sandbox
        FOREIGN KEY (id, current_allocation_id) REFERENCES allocations (sandbox_id, id),
    ADD CONSTRAINT sandboxes_active_transition_same_sandbox
        FOREIGN KEY (id, active_transition_operation_id) REFERENCES operations (sandbox_id, id);
