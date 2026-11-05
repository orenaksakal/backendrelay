drop table if exists execution_task_deployments;
alter table execution_deployments drop column if exists kind;
