-- Commands retain their original allocation even after destroy clears the sandbox pointer.
-- Nullable for pre-existing generic operation rows; those rows cannot be dispatched
-- by the execution store. New execute admission always pins a concrete allocation.
ALTER TABLE operations
    ADD COLUMN execution_allocation_id uuid,
    ADD CONSTRAINT operations_execution_allocation_same_sandbox
        FOREIGN KEY (sandbox_id, execution_allocation_id)
        REFERENCES allocations (sandbox_id, id),
    ADD CONSTRAINT operations_execution_allocation_kind
        CHECK (execution_allocation_id IS NULL OR kind = 'execute');

-- The current guest runner owns one command at a time. Unknown is still active.
CREATE UNIQUE INDEX operations_one_active_execution
    ON operations (sandbox_id)
    WHERE kind = 'execute' AND execution_allocation_id IS NOT NULL
      AND status IN ('queued', 'running', 'unknown');
