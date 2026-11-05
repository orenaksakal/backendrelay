begin;

alter table execution_crons drop constraint execution_crons_timezone_check;
alter table execution_crons alter column timezone set default 'UTC';
alter table execution_crons add constraint execution_crons_timezone_check
  check (char_length(timezone) between 1 and 128);

commit;
