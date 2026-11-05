create table execution_secrets (
  app_id uuid not null references apps (id) on delete cascade,
  name text not null check (name ~ '^[A-Z][A-Z0-9_]{0,63}$'),
  version uuid not null unique,
  ciphertext bytea not null,
  value_bytes integer not null check (value_bytes between 0 and 16384),
  created_at timestamptz not null default now(),
  updated_at timestamptz not null default now(),
  primary key (app_id, name)
);
alter table execution_deployments add column secret_snapshot bytea;
