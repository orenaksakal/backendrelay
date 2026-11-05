delete from app_email_senders where email_provider = 'cloudflare';

alter table app_email_senders
  drop constraint app_email_senders_email_provider_check;

alter table app_email_senders
  add constraint app_email_senders_email_provider_check
  check (email_provider in ('postmark', 'ses'));

drop table app_sender_domains;
