create table execution_deployments (
  id uuid primary key,
  app_id uuid not null references apps (id) on delete cascade,
  unique (app_id, id),
  source jsonb not null check (octet_length(source::text) <= 524288),
  entrypoint text not null check (octet_length(entrypoint) between 1 and 1024),
  access text not null default 'admin' check (access in ('admin', 'authenticated', 'public')),
  status text not null check (status in ('queued', 'building', 'ready', 'failed', 'disabled', 'disabling')),
  idempotency_key text check (idempotency_key is null or octet_length(idempotency_key) <= 256),
  build_error text check (build_error is null or octet_length(build_error) <= 65536),
  build_logs text check (build_logs is null or octet_length(build_logs) <= 65536),
  retry_at timestamptz not null default now(),
  attempts integer not null default 0 check (attempts >= 0 and attempts <= 100),
  runner_deleted_at timestamptz,
  created_at timestamptz not null default now(),
  updated_at timestamptz not null default now()
);
create unique index execution_deployments_idempotency on execution_deployments (app_id, idempotency_key) where idempotency_key is not null;
create index execution_deployments_app_created on execution_deployments (app_id, created_at desc, id desc);

create table execution_route_apps (
  app_id uuid primary key references apps (id) on delete cascade,
  deployment_id uuid not null,
  created_at timestamptz not null default now(),
  updated_at timestamptz not null default now(),
  foreign key (app_id, deployment_id) references execution_deployments (app_id, id) on delete cascade
);
