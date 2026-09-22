-- Retain the original claim deadline after releasing its active lease. Completion
-- is physical metadata evidence, not root forgetting or host-capacity recovery.
ALTER TABLE allocation_retirements
    ADD COLUMN metadata_completion jsonb,
    ADD COLUMN metadata_completed_at timestamptz,
    ADD COLUMN metadata_claim_expires_at timestamptz,
    ADD CONSTRAINT allocation_retirement_completion_shape CHECK (
        (metadata_completion IS NULL AND metadata_completed_at IS NULL AND metadata_claim_expires_at IS NULL)
        OR (
            metadata_completion IS NOT NULL AND metadata_completed_at IS NOT NULL
            AND metadata_claim_expires_at IS NOT NULL AND lease_expires_at IS NULL
            AND claim_revision > 0
            AND jsonb_typeof(metadata_completion) = 'object'
            AND octet_length(metadata_completion::text) <= 16384
            AND COALESCE(
                jsonb_typeof(metadata_completion->'observed_unix_ms') = 'number'
                AND (metadata_completion->>'observed_unix_ms')::numeric > 0
                AND (metadata_completion->>'observed_unix_ms')::numeric = trunc((metadata_completion->>'observed_unix_ms')::numeric)
                AND (metadata_completion->>'observed_unix_ms')::numeric <= floor(extract(epoch FROM metadata_claim_expires_at)*1000)
                AND metadata_completion->'version' = '1'::jsonb
                AND metadata_completion#>'{request,intent}' = intent
                AND metadata_completion#>'{request,intent,simulated}' = 'false'::jsonb
                AND metadata_completion#>'{request,revision}' = to_jsonb(claim_revision)
                AND metadata_completion#>'{request,reporting_epoch}' = to_jsonb(reporting_epoch)
                AND metadata_completion#>'{request,expires_unix_ms}' = to_jsonb(floor(extract(epoch FROM metadata_claim_expires_at)*1000)::bigint),
                false
            )
        )
    );
