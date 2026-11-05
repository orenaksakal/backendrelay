alter table execution_deployments drop column if exists secret_snapshot;
drop table if exists execution_secrets;
