-- The native worker polls only unclaimed jobs, ordered by creation and ID.
-- Preserve triples_app_id: its usage must be assessed on the actual deployment.
CREATE INDEX indexing_jobs_available
  ON indexing_jobs (created_at, id)
  WHERE worker_id IS NULL AND job_status IN ('waiting', 'processing');
