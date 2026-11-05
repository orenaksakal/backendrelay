do $$
begin
  if exists (select 1 from execution_crons where timezone <> 'UTC') then
    raise exception 'cannot roll back cron timezones while non-UTC schedules exist';
  end if;

  alter table execution_crons drop constraint execution_crons_timezone_check;
  alter table execution_crons alter column timezone drop default;
  alter table execution_crons add constraint execution_crons_timezone_check check (timezone = 'UTC');
end;
$$;
