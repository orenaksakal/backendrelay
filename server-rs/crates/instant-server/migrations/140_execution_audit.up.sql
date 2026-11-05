create table execution_audit_events (
  id uuid primary key default gen_random_uuid(),
  app_id uuid not null references apps(id) on delete cascade,
  actor_type text not null check (actor_type in ('admin_token', 'dashboard', 'control_plane')),
  actor_id uuid,
  action text not null check (action in (
    'tasks.register', 'workflows.register', 'crons.upsert', 'pause', 'resume', 'run', 'delete',
    'queues.pause', 'queues.resume', 'secrets.set', 'secrets.delete', 'deployments.create',
    'deployments.activate', 'deployments.disable', 'runs.cancel', 'runs.retry', 'limits.update'
  )),
  resource text not null check (
    octet_length(resource) between 1 and 256 and resource !~ '[[:cntrl:]]'
  ),
  details jsonb not null default '{}'::jsonb check (
    jsonb_typeof(details) = 'object' and octet_length(details::text) <= 2048
  ),
  created_at timestamptz not null default now()
);

create index execution_audit_events_app_created
  on execution_audit_events (app_id, created_at desc, id desc);

create index execution_audit_events_retention
  on execution_audit_events (created_at, id);

create function execution_audit_mark_app_deleting() returns trigger language plpgsql as $$
begin
  perform set_config('instant.execution_audit_deleting_app', old.id::text, true);
  return old;
end;
$$;

create trigger apps_mark_execution_audit_deleting
  before delete on apps
  for each row execute function execution_audit_mark_app_deleting();

create function execution_audit_management_change() returns trigger language plpgsql as $$
declare
  old_row jsonb;
  new_row jsonb;
  row_data jsonb;
  event_action text;
  event_resource text;
  event_details jsonb := '{}'::jsonb;
  audit_actor text := current_setting('instant.execution_audit_actor', true);
  audit_actor_type text := 'control_plane';
  audit_actor_id uuid;
  app uuid;
begin
  if tg_op = 'UPDATE' then
    old_row := to_jsonb(old);
  end if;
  if tg_op = 'DELETE' then
    old_row := to_jsonb(old);
    row_data := old_row;
  else
    new_row := to_jsonb(new);
    row_data := new_row;
  end if;
  app := (row_data ->> 'app_id')::uuid;

  -- App deletion cascades management rows. Those are not independent operations.
  if current_setting('instant.execution_audit_deleting_app', true) = app::text then
    if tg_op = 'DELETE' then return old; else return new; end if;
  end if;

  if audit_actor = 'admin_token' then
    audit_actor_type := 'admin_token';
  elsif audit_actor = 'dashboard' then
    audit_actor_type := 'dashboard';
  elsif audit_actor ~ '^dashboard:[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$' then
    audit_actor_type := 'dashboard';
    audit_actor_id := split_part(audit_actor, ':', 2)::uuid;
  end if;

  if tg_table_name = 'execution_task_definitions' and tg_op = 'INSERT' then
    event_action := 'tasks.register';
    event_resource := row_data ->> 'name';
    event_details := jsonb_build_object('version', row_data ->> 'version');
  elsif tg_table_name = 'execution_workflow_definitions' and tg_op = 'INSERT' then
    event_action := 'workflows.register';
    event_resource := row_data ->> 'name';
    event_details := jsonb_build_object('version', row_data ->> 'version');
  elsif tg_table_name = 'execution_crons' then
    event_resource := row_data ->> 'name';
    if tg_op = 'DELETE' then
      event_action := 'delete';
    elsif tg_op = 'INSERT' then
      event_action := 'crons.upsert';
    elsif new_row -> 'paused_at' is distinct from old_row -> 'paused_at' then
      event_action := case when new_row ->> 'paused_at' is null then 'resume' else 'pause' end;
    elsif new_row -> 'cron' is distinct from old_row -> 'cron'
       or new_row -> 'timezone' is distinct from old_row -> 'timezone'
       or new_row -> 'target' is distinct from old_row -> 'target'
       or new_row -> 'overlap' is distinct from old_row -> 'overlap'
       or new_row -> 'misfire' is distinct from old_row -> 'misfire' then
      event_action := 'crons.upsert';
    end if;
  elsif tg_table_name = 'execution_secrets' then
    event_resource := row_data ->> 'name';
    event_action := case when tg_op = 'DELETE' then 'secrets.delete' else 'secrets.set' end;
    event_details := jsonb_build_object('version', row_data ->> 'version');
  elsif tg_table_name = 'execution_deployments' and tg_op = 'INSERT' then
    event_action := 'deployments.create';
    event_resource := row_data ->> 'id';
    event_details := jsonb_build_object('deployment_id', row_data ->> 'id');
  elsif tg_table_name = 'execution_deployments'
     and tg_op = 'UPDATE'
     and old_row ->> 'status' is distinct from new_row ->> 'status'
     and new_row ->> 'status' = 'disabled' then
    event_action := 'deployments.disable';
    event_resource := row_data ->> 'id';
    event_details := jsonb_build_object('deployment_id', row_data ->> 'id');
  elsif tg_table_name = 'execution_route_apps' then
    event_action := case when tg_op = 'DELETE' then 'deployments.disable' else 'deployments.activate' end;
    event_resource := row_data ->> 'deployment_id';
    event_details := jsonb_build_object('deployment_id', row_data ->> 'deployment_id');
  elsif tg_table_name = 'execution_task_deployments'
     and (tg_op = 'INSERT' or old_row -> 'enabled' is distinct from new_row -> 'enabled') then
    event_action := case when row_data ->> 'enabled' = 'true' then 'deployments.activate' else 'deployments.disable' end;
    event_resource := row_data ->> 'deployment_id';
    event_details := jsonb_build_object('deployment_id', row_data ->> 'deployment_id', 'version', row_data ->> 'task_version');
  elsif tg_table_name = 'execution_cron_manual_runs' and tg_op = 'INSERT' then
    event_action := 'run';
    event_resource := row_data ->> 'cron_name';
    event_details := jsonb_build_object('run_id', row_data ->> 'run_id');
  elsif tg_table_name = 'execution_runs' and tg_op = 'INSERT' and new_row -> 'request' <> '{}'::jsonb then
    event_action := case when (new_row -> 'request') ->> 'source' = 'retry' then 'runs.retry' else 'run' end;
    event_resource := coalesce(new_row ->> 'task_name', new_row ->> 'workflow_name');
    event_details := jsonb_build_object('run_id', new_row ->> 'id');
  elsif tg_table_name = 'execution_runs'
     and tg_op = 'UPDATE'
     and old_row ->> 'cancel_requested_at' is null
     and new_row ->> 'cancel_requested_at' is not null then
    event_action := 'runs.cancel';
    event_resource := coalesce(new_row ->> 'task_name', new_row ->> 'workflow_name');
    event_details := jsonb_build_object('run_id', new_row ->> 'id');
  elsif tg_table_name = 'execution_app_limits' then
    event_action := 'limits.update';
    event_resource := 'app';
  end if;

  if event_action is not null then
    insert into execution_audit_events (app_id, actor_type, actor_id, action, resource, details)
    values (app, audit_actor_type, audit_actor_id, event_action, event_resource, event_details);
  end if;
  if tg_op = 'DELETE' then return old; else return new; end if;
end;
$$;

create trigger execution_audit_task_definitions
  after insert on execution_task_definitions
  for each row execute function execution_audit_management_change();
create trigger execution_audit_workflow_definitions
  after insert on execution_workflow_definitions
  for each row execute function execution_audit_management_change();
create trigger execution_audit_crons
  after insert or update or delete on execution_crons
  for each row execute function execution_audit_management_change();
create trigger execution_audit_secrets
  after insert or update or delete on execution_secrets
  for each row execute function execution_audit_management_change();
create trigger execution_audit_deployments
  after insert or update on execution_deployments
  for each row execute function execution_audit_management_change();
create trigger execution_audit_route_apps
  after insert or update or delete on execution_route_apps
  for each row execute function execution_audit_management_change();
create trigger execution_audit_task_deployments
  after insert or update on execution_task_deployments
  for each row execute function execution_audit_management_change();
create trigger execution_audit_cron_manual_runs
  after insert on execution_cron_manual_runs
  for each row execute function execution_audit_management_change();
create trigger execution_audit_runs
  after insert or update on execution_runs
  for each row execute function execution_audit_management_change();
create trigger execution_audit_app_limits
  after insert or update on execution_app_limits
  for each row execute function execution_audit_management_change();
