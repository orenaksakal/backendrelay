alter table execution_task_definitions
  drop column if exists invocation_policy;

drop table if exists execution_rate_limit_counters;
drop table if exists execution_app_limits;
