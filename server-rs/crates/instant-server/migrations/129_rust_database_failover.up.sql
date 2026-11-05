create table rust_database_failovers (
  id uuid primary key,
  leader_machine_id uuid not null,
  source_database_id text not null,
  target_database_id text not null,
  phase text not null check (
    phase in (
      'wait-for-followers', 'pause-writes', 'cancel-in-progress',
      'verify-catch-up', 'resume', 'completed', 'rollback', 'failed'
    )
  ),
  marker_transaction_id bigint,
  error text,
  created_at timestamptz not null default now(),
  updated_at timestamptz not null default now()
);

create unique index rust_database_failovers_one_active_idx
  on rust_database_failovers ((true))
  where phase not in ('completed', 'rollback', 'failed');

create trigger update_updated_at_trigger
before update on rust_database_failovers
for each row
execute function update_updated_at_column();

create table rust_database_failover_participants (
  failover_id uuid not null references rust_database_failovers (id) on delete cascade,
  machine_id uuid not null,
  status text not null check (
    status in (
      'pending', 'ready', 'pause-writes', 'cancel-in-progress', 'resume', 'rollback', 'failed'
    )
  ),
  error text,
  created_at timestamptz not null default now(),
  updated_at timestamptz not null default now(),
  primary key (failover_id, machine_id)
);

create index rust_database_failover_participants_updated_idx
  on rust_database_failover_participants (failover_id, updated_at);

create trigger update_updated_at_trigger
before update on rust_database_failover_participants
for each row
execute function update_updated_at_column();
