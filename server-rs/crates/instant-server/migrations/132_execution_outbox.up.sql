create table execution_outbox (
  id uuid primary key,
  app_id uuid not null references apps (id) on delete cascade,
  owner_id uuid,
  task_name text not null check (octet_length(task_name) between 1 and 128),
  task_version text not null check (octet_length(task_version) between 1 and 128),
  input jsonb not null,
  idempotency_key text check (idempotency_key is null or octet_length(idempotency_key) <= 256),
  available_at timestamptz not null,
  next_attempt_at timestamptz not null default now(),
  max_attempts smallint not null check (max_attempts between 1 and 100),
  delivery_attempts integer not null default 0 check (delivery_attempts >= 0),
  lease_token uuid,
  lease_expires_at timestamptz,
  delivered_at timestamptz,
  blocked_at timestamptz,
  last_error text,
  created_at timestamptz not null default now(),
  updated_at timestamptz not null default now(),
  check (
    (lease_token is null and lease_expires_at is null)
    or (lease_token is not null and lease_expires_at is not null)
  )
);

create unique index execution_outbox_idempotency
  on execution_outbox (app_id, task_name, task_version, idempotency_key)
  where idempotency_key is not null;

create index execution_outbox_pending
  on execution_outbox (next_attempt_at, created_at, id)
  where delivered_at is null and blocked_at is null;

create table execution_task_definitions (
  app_id uuid not null references apps (id) on delete cascade,
  name text not null check (octet_length(name) between 1 and 128),
  version text not null check (octet_length(version) between 1 and 128),
  access text not null default 'admin' check (access in ('admin', 'authenticated')),
  input_schema jsonb not null default '{}'::jsonb,
  max_attempts smallint not null default 3 check (max_attempts between 1 and 100),
  timeout_seconds integer not null default 300 check (timeout_seconds between 1 and 3600),
  concurrency smallint not null default 1 check (concurrency between 1 and 100),
  created_at timestamptz not null default now(),
  updated_at timestamptz not null default now(),
  primary key (app_id, name, version)
);

create index execution_task_definitions_current
  on execution_task_definitions (app_id, name, created_at desc, version desc);

create table execution_runs (
  id uuid primary key,
  app_id uuid not null references apps (id) on delete cascade,
  owner_id uuid,
  kind text not null check (kind in ('task', 'workflow')),
  task_name text,
  task_version text,
  workflow_name text,
  workflow_version text,
  input jsonb not null,
  request jsonb not null default '{}'::jsonb,
  result jsonb,
  error text,
  status text not null check (status in ('pending_dispatch', 'queued', 'running', 'waiting', 'completed', 'failed', 'cancelled')),
  idempotency_key text check (idempotency_key is null or octet_length(idempotency_key) <= 256),
  cancel_requested_at timestamptz,
  attempt_count smallint not null default 0 check (attempt_count between 0 and 100),
  created_at timestamptz not null default now(),
  updated_at timestamptz not null default now(),
  completed_at timestamptz,
  check ((kind = 'task' and task_name is not null and task_version is not null and workflow_name is null and workflow_version is null) or (kind = 'workflow' and workflow_name is not null and workflow_version is not null and task_name is null and task_version is null))
);

create unique index execution_runs_idempotency
  on execution_runs (app_id, kind, coalesce(task_name, workflow_name), owner_id, idempotency_key) nulls not distinct
  where idempotency_key is not null;
create index execution_runs_by_app on execution_runs (app_id, created_at desc, id desc);
create index execution_runs_by_owner on execution_runs (app_id, owner_id, created_at desc, id desc) where owner_id is not null;

create table execution_workflow_definitions (
  app_id uuid not null references apps (id) on delete cascade,
  name text not null check (octet_length(name) between 1 and 128),
  version text not null check (octet_length(version) between 1 and 128),
  access text not null default 'admin' check (access in ('admin', 'authenticated')),
  input_schema jsonb not null default '{}'::jsonb,
  steps jsonb not null,
  created_at timestamptz not null default now(),
  updated_at timestamptz not null default now(),
  primary key (app_id, name, version)
);

create table execution_workflow_state (
  run_id uuid primary key references execution_runs (id) on delete cascade,
  step_index integer not null default 0 check (step_index >= 0),
  state jsonb not null default '{}'::jsonb,
  current_child_id uuid references execution_runs (id) on delete set null,
  wake_at timestamptz,
  waiting_signal text,
  signal_deadline timestamptz,
  updated_at timestamptz not null default now()
);
create index execution_workflow_wakeups on execution_workflow_state (wake_at, run_id) where wake_at is not null;
create index execution_workflow_signal_waits on execution_workflow_state (waiting_signal, signal_deadline, run_id) where waiting_signal is not null;

create table execution_workflow_signals (
  id uuid primary key,
  run_id uuid not null references execution_runs (id) on delete cascade,
  signal text not null check (octet_length(signal) between 1 and 128),
  data jsonb not null,
  idempotency_key text check (idempotency_key is null or octet_length(idempotency_key) <= 256),
  created_at timestamptz not null default now(),
  consumed_at timestamptz
);
create unique index execution_workflow_signals_idempotency
  on execution_workflow_signals (run_id, signal, idempotency_key) where idempotency_key is not null;
create index execution_workflow_signals_pending
  on execution_workflow_signals (run_id, signal, created_at, id) where consumed_at is null;

create table execution_crons (
  app_id uuid not null references apps (id) on delete cascade,
  name text not null check (octet_length(name) between 1 and 128),
  cron text not null check (octet_length(cron) between 1 and 256),
  timezone text not null check (timezone = 'UTC'),
  target jsonb not null,
  overlap text not null default 'skip' check (overlap in ('skip')),
  misfire text not null default 'coalesce' check (misfire in ('coalesce')),
  paused_at timestamptz,
  next_at timestamptz not null,
  created_at timestamptz not null default now(),
  updated_at timestamptz not null default now(),
  primary key (app_id, name)
);
create index execution_crons_due on execution_crons (next_at, app_id, name) where paused_at is null;

create table execution_cron_occurrences (
  app_id uuid not null,
  name text not null,
  occurred_at timestamptz not null,
  run_id uuid not null references execution_runs (id) on delete cascade,
  primary key (app_id, name, occurred_at),
  foreign key (app_id, name) references execution_crons (app_id, name) on delete cascade
);
