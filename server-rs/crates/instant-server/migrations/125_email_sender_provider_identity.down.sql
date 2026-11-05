DELETE FROM app_email_senders
WHERE email_provider = 'ses' OR postmark_id IS NULL;

ALTER TABLE app_email_senders
  DROP CONSTRAINT app_email_senders_email_provider_check,
  DROP COLUMN provider_id,
  DROP COLUMN email_provider,
  ALTER COLUMN postmark_id SET NOT NULL;
