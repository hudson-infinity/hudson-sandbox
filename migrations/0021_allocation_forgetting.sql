-- Forgetting uses separate claims; the metadata-completion request never changes.
ALTER TABLE allocation_retirements
    ADD COLUMN forget_revision bigint NOT NULL DEFAULT 0 CHECK (forget_revision>=0),
    ADD COLUMN forget_epoch bigint CHECK (forget_epoch>0),
    ADD COLUMN forget_lease_expires_at timestamptz,
    ADD COLUMN forget_completion jsonb,
    ADD COLUMN forgotten_at timestamptz,
    ADD COLUMN forget_claim_expires_at timestamptz,
    ADD CONSTRAINT allocation_forget_claim_shape CHECK (
        (forget_revision=0 AND forget_epoch IS NULL AND forget_lease_expires_at IS NULL)
        OR (forget_revision>0 AND forget_epoch IS NOT NULL AND metadata_completion IS NOT NULL)
    ),
    ADD CONSTRAINT allocation_forget_completion_shape CHECK (
        (forget_completion IS NULL AND forgotten_at IS NULL AND forget_claim_expires_at IS NULL)
        OR (forget_completion IS NOT NULL AND forgotten_at IS NOT NULL AND forget_claim_expires_at IS NOT NULL
            AND forget_lease_expires_at IS NULL AND forget_revision>0 AND metadata_completion IS NOT NULL
            AND jsonb_typeof(forget_completion)='object' AND octet_length(forget_completion::text)<=16384
            AND COALESCE(
                forget_completion->'version'='1'::jsonb
                AND forget_completion->'state' IN ('1'::jsonb,'2'::jsonb)
                AND forget_completion#>'{request,version}'='1'::jsonb
                AND forget_completion#>'{request,metadata_request}'=metadata_completion->'request'
                AND forget_completion#>'{request,claim,intent}'=intent
                AND forget_completion#>'{request,claim,revision}'=to_jsonb(forget_revision)
                AND forget_completion#>'{request,claim,reporting_epoch}'=to_jsonb(forget_epoch)
                AND forget_completion#>'{request,claim,expires_unix_ms}'=to_jsonb(floor(extract(epoch FROM forget_claim_expires_at)*1000)::bigint)
                AND jsonb_typeof(forget_completion->'observed_unix_ms')='number'
                AND (forget_completion->>'observed_unix_ms')::numeric>0
                AND (forget_completion->>'observed_unix_ms')::numeric=trunc((forget_completion->>'observed_unix_ms')::numeric)
                AND (forget_completion->>'observed_unix_ms')::numeric<=floor(extract(epoch FROM forget_claim_expires_at)*1000),
                false)
        )
    );
