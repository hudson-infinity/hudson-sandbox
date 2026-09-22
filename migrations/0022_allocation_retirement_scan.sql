-- Scheduling metadata is separate from the immutable retirement proof and its
-- delivery claims. A failed/ineligible candidate must not starve later owners.
ALTER TABLE allocations ADD COLUMN retirement_scan_at timestamptz;
CREATE INDEX allocations_retirement_scan
    ON allocations(host_id, retirement_scan_at NULLS FIRST, id)
    WHERE status = 'released';
