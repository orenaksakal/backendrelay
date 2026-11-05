create table rust_realtime_event_consumers (
  machine_id uuid not null references rust_realtime_machines (machine_id) on delete cascade,
  consumer_kind text not null check (consumer_kind in ('runtime', 'admin')),
  event_cursor bigint not null check (event_cursor >= 0),
  created_at timestamptz not null default now(),
  updated_at timestamptz not null default now(),
  primary key (machine_id, consumer_kind)
);

create index rust_realtime_event_consumers_cursor_idx
  on rust_realtime_event_consumers (event_cursor);

create trigger update_updated_at_trigger
before update on rust_realtime_event_consumers
for each row
execute function update_updated_at_column();
