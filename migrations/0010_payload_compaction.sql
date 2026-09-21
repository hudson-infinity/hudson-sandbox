-- Operator opt-in compaction leaves identity, outcomes, retry digests and all
-- execution/retirement receipts intact. Upgrade itself removes nothing.
ALTER TABLE operations
    ADD COLUMN payload_compacted_at timestamptz,
    ADD COLUMN command_summary jsonb,
    ADD COLUMN payload_compaction_next_at timestamptz,
    ADD CONSTRAINT operations_payload_compaction CHECK (
        (payload_compacted_at IS NULL AND command_summary IS NULL) OR
        (payload_compacted_at IS NOT NULL
         AND status IN ('succeeded','failed','cancelled') AND completed_at IS NOT NULL
         AND response_expires_at IS NOT NULL AND payload_compacted_at>=response_expires_at
         AND payload='{}'::jsonb AND result IS NULL AND error IS NULL
         AND lease_expires_at IS NULL AND next_retry_at IS NULL
         AND output_lease_expires_at IS NULL AND output_next_retry_at IS NULL
         AND payload_compaction_next_at IS NULL AND output_status IN ('none','expired')
         AND CASE WHEN kind='execute' THEN command_summary IS NOT NULL AND jsonb_typeof(command_summary)='object'
                  ELSE command_summary IS NULL END)
    );

-- An unissued ticket grants no upload authority. Compaction may atomically
-- fence pending publication and expire that attempt without inventing a ticket.
ALTER TABLE operations DROP CONSTRAINT operations_output_metadata;
ALTER TABLE operations ADD CONSTRAINT operations_output_metadata CHECK (
    CASE output_status
    WHEN 'none' THEN output_ticket IS NULL AND output_plan IS NULL AND output_expires_at IS NULL AND output_refs='[]'::jsonb
    WHEN 'pending' THEN output_ticket IS NULL AND output_plan IS NULL AND output_expires_at IS NULL AND output_refs='[]'::jsonb
    WHEN 'uploading' THEN output_ticket IS NOT NULL AND output_expires_at IS NOT NULL AND output_refs='[]'::jsonb
    WHEN 'published' THEN output_ticket IS NOT NULL AND output_plan IS NOT NULL AND output_expires_at IS NOT NULL
        AND CASE WHEN jsonb_typeof(output_refs)='array' THEN jsonb_array_length(output_refs)=2 ELSE false END
    WHEN 'expired' THEN output_expires_at IS NOT NULL AND
        (output_ticket IS NOT NULL OR
         (payload_compacted_at IS NOT NULL AND output_plan IS NULL AND output_refs='[]'::jsonb))
    ELSE false END
);
CREATE INDEX operations_payload_compaction_idx ON operations (response_expires_at,id)
WHERE payload_compacted_at IS NULL AND response_expires_at IS NOT NULL
AND status IN ('succeeded','failed','cancelled');
