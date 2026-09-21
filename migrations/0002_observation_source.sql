-- Preserve the origin of an observation at the public sandbox boundary.
-- NULL means no source has been confirmed; false is real, true is simulated.
ALTER TABLE sandboxes ADD COLUMN observation_simulated boolean;
