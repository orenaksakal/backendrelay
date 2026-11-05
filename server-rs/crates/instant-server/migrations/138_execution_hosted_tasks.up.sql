alter table execution_deployments
  add column kind text not null default 'routes' check (kind in ('routes', 'tasks'));

create table execution_task_deployments (
  app_id uuid not null,
  task_name text not null check (octet_length(task_name) between 1 and 128),
  task_version text not null check (octet_length(task_version) between 1 and 128),
  deployment_id uuid not null,
  enabled boolean not null default true,
  last_polled_at timestamptz not null default now(),
  created_at timestamptz not null default now(),
  updated_at timestamptz not null default now(),
  primary key (app_id, task_name, task_version),
  foreign key (app_id, deployment_id)
    references execution_deployments (app_id, id) on delete cascade
);

create index execution_task_deployments_claimable
  on execution_task_deployments (last_polled_at, deployment_id, app_id, task_name, task_version)
  where enabled;
