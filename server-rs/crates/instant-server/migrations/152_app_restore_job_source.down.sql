drop index if exists app_restore_jobs_source_app_id;
alter table app_restore_jobs drop column if exists source_app_id;
