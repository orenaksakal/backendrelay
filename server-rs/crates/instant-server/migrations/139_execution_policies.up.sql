create table execution_app_limits (
  app_id uuid primary key references apps (id) on delete cascade,
  max_outstanding integer not null default 10000 check (max_outstanding between 1 and 10000),
  max_running integer not null default 100 check (max_running between 1 and 100),
  submissions_per_minute integer not null default 600 check (submissions_per_minute between 1 and 600),
  broker_operations_per_minute integer not null default 6000 check (broker_operations_per_minute between 1 and 6000),
  updated_at timestamptz not null default now()
);

-- One row per app and operation kind is overwritten as each minute rolls over,
-- so rate limiting cannot accumulate unbounded historical minute buckets.
create table execution_rate_limit_counters (
  app_id uuid not null references apps (id) on delete cascade,
  kind text not null check (kind in ('submission', 'broker_operation')),
  bucket timestamptz not null,
  tokens integer not null check (tokens >= 0),
  primary key (app_id, kind)
);

alter table execution_task_definitions
  add column invocation_policy jsonb;
