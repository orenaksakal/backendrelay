create table execution_cron_manual_runs (
  id uuid primary key,
  app_id uuid not null references apps (id) on delete cascade,
  cron_name text not null check (octet_length(cron_name) between 1 and 128),
  run_id uuid not null references execution_runs (id) on delete cascade,
  idempotency_key text check (idempotency_key is null or octet_length(idempotency_key) between 1 and 256),
  created_at timestamptz not null default now()
);

create unique index execution_cron_manual_runs_idempotency
  on execution_cron_manual_runs (app_id, cron_name, idempotency_key)
  where idempotency_key is not null;
create index execution_cron_manual_runs_by_cron
  on execution_cron_manual_runs (app_id, cron_name, created_at, id);
create index execution_cron_manual_runs_by_run
  on execution_cron_manual_runs (run_id);
