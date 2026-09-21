-- Bound admission's retained-history scan to one pinned allocation. This does
-- not release reservations, rewrite old rows or change any execution outcome.
CREATE INDEX operations_execution_capacity
    ON operations (execution_allocation_id, id)
    WHERE kind = 'execute';
