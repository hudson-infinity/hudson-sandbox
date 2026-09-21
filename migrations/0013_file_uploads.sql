ALTER TABLE operations
    ADD COLUMN file_allocation_id uuid,
    ADD CONSTRAINT operations_file_allocation_kind CHECK (file_allocation_id IS NULL OR kind='file_write'),
    ADD CONSTRAINT operations_file_allocation_same_sandbox FOREIGN KEY(sandbox_id,file_allocation_id) REFERENCES allocations(sandbox_id,id),
    ADD CONSTRAINT operations_file_pin_unique UNIQUE(id,file_allocation_id);
-- Private retained source/dispatch state; bytes remain in object storage.
CREATE TABLE file_uploads (
    operation_id uuid PRIMARY KEY,
    project_id uuid NOT NULL,
    sandbox_id uuid NOT NULL,
    allocation_id uuid NOT NULL,
    size bigint NOT NULL CHECK (size BETWEEN 0 AND 8388608),
    token_hash bytea NOT NULL CHECK (octet_length(token_hash)=32),
    plan jsonb NOT NULL CHECK (jsonb_typeof(plan)='object' AND (plan#>>'{upload,size}')::bigint=size),
    source_ref jsonb CHECK (source_ref IS NULL OR jsonb_typeof(source_ref)='object'),
    written bigint NOT NULL DEFAULT 0 CHECK (written>=0 AND written<=size),
    begin_requested boolean NOT NULL DEFAULT false,
    commit_requested boolean NOT NULL DEFAULT false,
    abort_requested boolean NOT NULL DEFAULT false,
    needs_inspect boolean NOT NULL DEFAULT false,
    record jsonb CHECK (record IS NULL OR jsonb_typeof(record)='object'),
    FOREIGN KEY (operation_id,allocation_id) REFERENCES operations(id,file_allocation_id),
    FOREIGN KEY (sandbox_id,operation_id) REFERENCES operations(sandbox_id,id),
    FOREIGN KEY (project_id,sandbox_id) REFERENCES sandboxes(project_id,id),
    FOREIGN KEY (sandbox_id,allocation_id) REFERENCES allocations(sandbox_id,id),
    CHECK (NOT begin_requested OR source_ref IS NOT NULL),
    CHECK (NOT commit_requested OR (begin_requested AND written=size)),
    CHECK (NOT abort_requested OR begin_requested),
    CHECK (begin_requested OR (written=0 AND record IS NULL AND NOT needs_inspect))
);
CREATE INDEX file_uploads_allocation_idx ON file_uploads(allocation_id);
CREATE INDEX file_uploads_project_idx ON file_uploads(project_id);
CREATE UNIQUE INDEX operations_one_active_file_write ON operations(sandbox_id)
WHERE kind='file_write' AND file_allocation_id IS NOT NULL AND status IN ('queued','running','unknown');
