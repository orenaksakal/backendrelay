create table rust_realtime_room_members (
  app_id uuid not null,
  room_id text not null,
  session_id uuid not null,
  machine_id uuid not null,
  instance_id text not null,
  user_id uuid,
  data jsonb not null default '{}'::jsonb,
  created_at timestamptz not null default now(),
  updated_at timestamptz not null default now(),
  primary key (app_id, room_id, session_id)
);

create index rust_realtime_room_members_machine_idx
  on rust_realtime_room_members (machine_id);

create trigger update_updated_at_trigger
before update on rust_realtime_room_members
for each row
execute function update_updated_at_column();

create table rust_realtime_room_events (
  id bigint generated always as identity primary key,
  source_machine_id uuid not null,
  kind text not null check (
    kind in (
      'presence', 'broadcast', 'stream-append', 'stream-disconnect', 'sse-push',
      'wal'
    )
  ),
  app_id uuid not null,
  room_id text not null,
  payload jsonb,
  created_at timestamptz not null default now()
);

create index rust_realtime_room_events_created_at_idx
  on rust_realtime_room_events (created_at);

create unique index rust_realtime_room_events_wal_idx
  on rust_realtime_room_events (kind, app_id, room_id)
  where kind = 'wal';

create table rust_realtime_sse_sessions (
  session_id uuid primary key,
  app_id uuid not null,
  machine_id uuid not null,
  token_hash bytea not null check (octet_length(token_hash) = 32),
  created_at timestamptz not null default now(),
  updated_at timestamptz not null default now()
);

create index rust_realtime_sse_sessions_machine_idx
  on rust_realtime_sse_sessions (machine_id);

create trigger update_updated_at_trigger
before update on rust_realtime_sse_sessions
for each row
execute function update_updated_at_column();

create table rust_realtime_machines (
  machine_id uuid primary key,
  created_at timestamptz not null default now(),
  updated_at timestamptz not null default now()
);

create index rust_realtime_machines_updated_at_idx
  on rust_realtime_machines (updated_at);

create table rust_realtime_stream_buffers (
  app_id uuid not null,
  stream_id uuid not null,
  machine_id uuid not null,
  content text not null default '',
  size bigint not null default 0 check (size >= 0),
  created_at timestamptz not null default now(),
  updated_at timestamptz not null default now(),
  primary key (app_id, stream_id),
  check (octet_length(content) = size)
);

create index rust_realtime_stream_buffers_machine_idx
  on rust_realtime_stream_buffers (machine_id);
