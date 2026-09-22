-- A retained freeze is not physical cleanup or a host deletion acknowledgement.
CREATE TABLE allocation_retirements (
    allocation_id uuid PRIMARY KEY REFERENCES allocation_permits(allocation_id),
    retirement_id uuid NOT NULL UNIQUE,
    intent jsonb NOT NULL CHECK (jsonb_typeof(intent)='object' AND octet_length(intent::text)<=8192),
    claim_revision bigint NOT NULL DEFAULT 0 CHECK (claim_revision>=0),
    reporting_epoch bigint NOT NULL CHECK (reporting_epoch>0),
    lease_expires_at timestamptz,
    prepared_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    CHECK (lease_expires_at IS NULL OR claim_revision>0),
    CHECK (intent->>'retirement' IS NOT NULL AND intent->>'retirement'='op_'||retirement_id::text),
    CHECK (intent->'permit'->>'allocation' IS NOT NULL AND intent->'permit'->>'allocation'='alc_'||allocation_id::text)
);
