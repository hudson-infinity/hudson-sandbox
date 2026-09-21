-- Only newly admitted cancellation requests use this phase. Existing generic
-- cancellation rows do not acquire authority through an upgrade.
CREATE UNIQUE INDEX operations_one_pending_command_cancel
    ON operations (target_operation_id)
    WHERE kind = 'cancel' AND phase = 'cancel_requested'
      AND status IN ('queued', 'running', 'unknown');
