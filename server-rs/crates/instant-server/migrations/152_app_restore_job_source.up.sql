-- compat: additive
-- A restore started from a stored backup records the app it came from so the
-- dashboard can list the job on both the source and the target app.
alter table app_restore_jobs
  add column if not exists source_app_id uuid references apps(id) on delete set null;

create index if not exists app_restore_jobs_source_app_id
  on app_restore_jobs(source_app_id)
  where source_app_id is not null;
