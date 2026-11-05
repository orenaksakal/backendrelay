-- Per-app custom sender domains for Cloudflare Email Sending. A domain is
-- usable only while its ownership is confirmed and Cloudflare reports it
-- enabled, so verification is stored per app and re-checked periodically.
create table app_sender_domains (
  app_id uuid not null references apps(id) on delete cascade,
  domain text not null,
  cloudflare_zone_id text,
  verified_at timestamptz,
  ownership_verified_at timestamptz,
  last_checked_at timestamptz not null default now(),
  last_error text,
  primary key (app_id, domain)
);

alter table app_email_senders
  drop constraint app_email_senders_email_provider_check;

alter table app_email_senders
  add constraint app_email_senders_email_provider_check
  check (email_provider in ('postmark', 'ses', 'cloudflare'));
