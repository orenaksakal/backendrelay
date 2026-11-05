CREATE TABLE app_backup_schedules (
  app_id uuid PRIMARY KEY REFERENCES apps(id) ON DELETE CASCADE,
  cron text NOT NULL CHECK (length(cron) BETWEEN 9 AND 128),
  enabled boolean NOT NULL DEFAULT true,
  next_run_at timestamptz NOT NULL,
  last_enqueued_at timestamptz,
  updated_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX app_backup_schedules_due ON app_backup_schedules(next_run_at) WHERE enabled;
