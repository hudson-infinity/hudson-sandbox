-- Keyset order is immutable creation time plus ID, always within a project.
CREATE INDEX sandboxes_project_created_idx ON sandboxes (project_id, created_at DESC, id DESC);
CREATE INDEX operations_project_created_idx ON operations (project_id, created_at DESC, id DESC);
CREATE INDEX operations_project_sandbox_created_idx ON operations (project_id, sandbox_id, created_at DESC, id DESC);
