-- Stable admission order survives clock rollback and both domain retirement floors.
ALTER TABLE allocations ADD COLUMN last_admitted_operation_id uuid;
ALTER TABLE allocations ADD COLUMN history_commands_scan_at timestamptz;
ALTER TABLE allocations ADD COLUMN history_files_scan_at timestamptz;
UPDATE allocations a SET last_admitted_operation_id=q.id FROM (
    SELECT DISTINCT ON (allocation_id) allocation_id,id FROM (
        SELECT execution_allocation_id AS allocation_id,id FROM operations WHERE execution_allocation_id IS NOT NULL
        UNION ALL
        SELECT file_allocation_id AS allocation_id,id FROM operations WHERE file_allocation_id IS NOT NULL
    ) pinned ORDER BY allocation_id,id DESC
) q WHERE a.id=q.allocation_id;

-- Rows retain the current reserved prefix and the last verified completed prefix.
-- Neither preparation nor a lease authorizes accounting refunds.
CREATE TABLE allocation_history (
    allocation_id uuid NOT NULL REFERENCES allocations(id),
    domain text NOT NULL CHECK (domain IN ('commands','files')),
    reserved_through uuid NOT NULL,
    completed_through uuid,
    claim_revision bigint NOT NULL DEFAULT 0 CHECK (claim_revision >= 0),
    lease_expires_at timestamptz,
    next_retry_at timestamptz,
    context jsonb CHECK (context IS NULL OR jsonb_typeof(context)='object'),
    request jsonb CHECK (request IS NULL OR jsonb_typeof(request)='object'),
    completion jsonb CHECK (completion IS NULL OR jsonb_typeof(completion)='object'),
    completed_at timestamptz,
    PRIMARY KEY (allocation_id,domain),
    CHECK (completed_through IS NULL OR completed_through<=reserved_through),
    CHECK (lease_expires_at IS NULL OR claim_revision>0),
    CHECK (request IS NULL OR (context IS NOT NULL AND claim_revision>0)),
    CHECK ((completed_through IS NULL AND completion IS NULL AND completed_at IS NULL)
        OR (completed_through IS NOT NULL AND completion IS NOT NULL AND completed_at IS NOT NULL AND context IS NOT NULL))
);
CREATE INDEX allocation_history_retry ON allocation_history(next_retry_at,allocation_id);
CREATE INDEX operations_file_history ON operations(file_allocation_id,id) WHERE kind='file_write' AND file_allocation_id IS NOT NULL;

-- Admission trusts only the acknowledged prefix, never the reserved one. Keep
-- the evidence/owner binding in one shared view for command and upload budgets.
-- Malformed or mismatched metadata remains charged (or fails the query closed).
CREATE VIEW completed_allocation_history AS
SELECT h.allocation_id,h.domain,h.completed_through
FROM allocation_history h JOIN allocations a ON a.id=h.allocation_id
WHERE h.completed_through IS NOT NULL
  AND h.completion->'completed'=h.completion->'request'->'barrier'
  AND h.completion->'completed'=jsonb_build_object(
    'version',1,'domain',CASE h.domain WHEN 'commands' THEN 1 ELSE 2 END,
    'context',h.context,'through','op_'||h.completed_through::text)
  AND h.context->>'allocation_id'='alc_'||a.id::text
  AND h.context->'generation'=to_jsonb(a.generation)
  AND jsonb_typeof(h.context->'boot_id')='string'
  AND length(h.context->>'boot_id') BETWEEN 1 AND 64
  AND (h.completion->'request'->'ownership')-ARRAY['revision','claim_expires_unix_ms']=jsonb_build_object(
    'host_id','hst_'||a.host_id::text,'project_id','prj_'||a.project_id::text,
    'sandbox_id','sbx_'||a.sandbox_id::text,'allocation_id','alc_'||a.id::text,
    'generation',a.generation,'supervisor_epoch',a.supervisor_epoch)
  AND jsonb_typeof(h.completion->'simulated')='boolean'
  AND (h.completion->'request'->'ownership'->>'revision')::numeric BETWEEN 1 AND h.claim_revision
  AND (h.completion->>'observed_unix_ms')::numeric>0
  AND (h.completion->>'observed_unix_ms')::numeric<=(h.completion->'request'->'ownership'->>'claim_expires_unix_ms')::numeric;
