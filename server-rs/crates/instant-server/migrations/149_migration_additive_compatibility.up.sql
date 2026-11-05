-- compat: additive
alter table instant_migration_checksums
  add column if not exists additive boolean not null default false;
