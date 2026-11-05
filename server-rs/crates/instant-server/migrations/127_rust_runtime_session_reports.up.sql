create table rust_runtime_session_reports (
  machine_id uuid primary key,
  report jsonb,
  session_count bigint,
  query_count bigint,
  proxied_connections jsonb,
  report_updated_at timestamptz,
  totals_updated_at timestamptz,
  proxy_updated_at timestamptz,
  created_at timestamptz not null default now(),
  updated_at timestamptz not null default now()
);

create index rust_runtime_session_reports_report_updated_at_idx
  on rust_runtime_session_reports (report_updated_at);

create index rust_runtime_session_reports_totals_updated_at_idx
  on rust_runtime_session_reports (totals_updated_at);

create trigger update_updated_at_trigger
before update on rust_runtime_session_reports
for each row
execute function update_updated_at_column();
