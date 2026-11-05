-- Explicit migration only; application startup never creates this table.
create table application_logs (
  id uuid primary key,
  app_id uuid not null references apps(id) on delete cascade,
  created_at timestamptz not null,
  source text not null check (source in ('http', 'realtime')),
  level text not null check (level in ('info', 'warning', 'error')),
  message text not null check (octet_length(message) <= 512),
  details jsonb not null check (octet_length(details::text) <= 2048)
);
create index application_logs_app_time on application_logs (app_id, created_at desc, id desc);
create index application_logs_retention on application_logs (created_at desc, id desc);
create index execution_runs_app_log_time on execution_runs (app_id, updated_at desc, id desc);
create index execution_deployments_app_log_time on execution_deployments (app_id, updated_at desc, id desc);
