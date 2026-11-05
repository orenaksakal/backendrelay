-- Dashboard refresh tokens were compared in plaintext and never expired.
-- Store the SHA-256 of the token text, keep the plaintext column until a later
-- migration drops it, and track use so sessions slide for 30 days.
alter table instant_user_refresh_tokens add column token_hash bytea;

update instant_user_refresh_tokens set token_hash = sha256(cast(cast(id as text) as bytea));

alter table instant_user_refresh_tokens alter column token_hash set not null;

create unique index instant_user_refresh_tokens_token_hash_key
  on instant_user_refresh_tokens (token_hash);

alter table instant_user_refresh_tokens
  add column last_used_at timestamptz not null default now();
