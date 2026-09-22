-- Destruction acknowledgement is not a guest acknowledgement and needs no boot.
CREATE TABLE released_allocation_history (
    allocation_id uuid NOT NULL REFERENCES allocations(id),
    domain text NOT NULL CHECK (domain IN ('commands','files')),
    start_after uuid,
    reserved_through uuid NOT NULL,
    completed_through uuid,
    context jsonb CHECK (context IS NULL OR jsonb_typeof(context)='object'),
    claim_revision bigint NOT NULL DEFAULT 0 CHECK (claim_revision>=0),
    reporting_epoch bigint NOT NULL CHECK (reporting_epoch>0),
    lease_expires_at timestamptz,
    next_retry_at timestamptz,
    request jsonb CHECK (request IS NULL OR jsonb_typeof(request)='object'),
    completion jsonb CHECK (completion IS NULL OR jsonb_typeof(completion)='object'),
    completed_at timestamptz,
    PRIMARY KEY(allocation_id,domain),
    CHECK (start_after IS NULL OR start_after<reserved_through),
    CHECK (completed_through IS NULL OR completed_through<=reserved_through),
    CHECK (completed_through IS NULL OR start_after IS NULL OR start_after<completed_through),
    CHECK (lease_expires_at IS NULL OR claim_revision>0),
    CHECK ((completed_through IS NULL AND completion IS NULL AND completed_at IS NULL)
        OR (completed_through IS NOT NULL AND completion IS NOT NULL AND completed_at IS NOT NULL))
);
CREATE INDEX released_history_retry ON released_allocation_history(next_retry_at,allocation_id);

ALTER VIEW completed_allocation_history RENAME TO completed_live_allocation_history;
CREATE VIEW completed_released_allocation_history AS
SELECT h.allocation_id,h.domain,h.completed_through
FROM released_allocation_history h JOIN allocations a ON a.id=h.allocation_id JOIN hosts host ON host.id=a.host_id
WHERE h.completed_through IS NOT NULL AND a.status='released' AND a.released_at IS NOT NULL
  AND h.completion->>'completed_through'='op_'||h.completed_through::text
  AND h.completion->'request'->>'through'='op_'||h.completed_through::text
  AND h.completion->'request'->'domain'=to_jsonb(CASE h.domain WHEN 'commands' THEN 1 ELSE 2 END)
  AND (h.completion->'request'->'ownership')-ARRAY['revision','claim_expires_unix_ms']=jsonb_build_object(
    'host_id','hst_'||a.host_id::text,'project_id','prj_'||a.project_id::text,
    'sandbox_id','sbx_'||a.sandbox_id::text,'allocation_id','alc_'||a.id::text,
    'generation',a.generation,'supervisor_epoch',a.supervisor_epoch)
  AND (h.completion->'request'->>'reporting_epoch')::numeric BETWEEN a.supervisor_epoch AND h.reporting_epoch
  AND (h.completion->'request'->>'reporting_epoch')::numeric<=host.supervisor_epoch
  AND h.completion->'release_state' IN ('3'::jsonb,'4'::jsonb)
  AND jsonb_typeof(h.completion->'simulated')='boolean'
  AND (h.completion->'request'->'ownership'->>'revision')::numeric BETWEEN 1 AND h.claim_revision
  AND (h.completion->>'observed_unix_ms')::numeric>0
  AND (h.completion->>'observed_unix_ms')::numeric<=(h.completion->'request'->'ownership'->>'claim_expires_unix_ms')::numeric;

-- One row per allocation/domain prevents join multiplication and double refunds.
CREATE VIEW completed_allocation_history AS
SELECT DISTINCT ON (allocation_id,domain) allocation_id,domain,completed_through FROM (
    SELECT * FROM completed_live_allocation_history
    UNION ALL SELECT * FROM completed_released_allocation_history
) verified ORDER BY allocation_id,domain,completed_through DESC;
