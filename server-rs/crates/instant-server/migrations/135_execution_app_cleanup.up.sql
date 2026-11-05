create table execution_app_cleanup_tombstones (
  app_id uuid primary key,
  deployment_ids uuid[] not null default '{}',
  lease_token uuid,
  lease_expires_at timestamptz,
  next_attempt_at timestamptz not null default now(),
  completed_at timestamptz,
  created_at timestamptz not null default now(),
  updated_at timestamptz not null default now(),
  check ((lease_token is null) = (lease_expires_at is null))
);

create index execution_app_cleanup_tombstones_pending
  on execution_app_cleanup_tombstones (next_attempt_at, created_at, app_id)
  where completed_at is null;

create function capture_execution_app_cleanup_tombstone() returns trigger language plpgsql as $$
begin
  if exists (select 1 from execution_runs where app_id = old.id)
     or exists (select 1 from execution_outbox where app_id = old.id)
     or exists (select 1 from execution_deployments where app_id = old.id) then
    insert into execution_app_cleanup_tombstones (app_id, deployment_ids)
    select old.id, coalesce(array_agg(id), '{}'::uuid[])
      from execution_deployments where app_id = old.id
    on conflict (app_id) do nothing;
  end if;
  return old;
end;
$$;

create trigger apps_capture_execution_cleanup
  before delete on apps
  for each row execute function capture_execution_app_cleanup_tombstone();
