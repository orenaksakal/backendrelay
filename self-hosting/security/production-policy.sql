-- Read-only, bounded projection of the *effective* live magic-code configuration.
-- Keep default/toggle semantics in sync with PostgresMagicCodeRateLimiter.
BEGIN READ ONLY;
SET LOCAL statement_timeout = '5s';
WITH settings AS (
  SELECT setting_attr.etype, setting.value AS setting, value.value
  FROM attrs setting_attr
  JOIN triples setting ON setting.app_id = setting_attr.app_id AND setting.attr_id = setting_attr.id
  JOIN attrs value_attr ON value_attr.app_id = setting_attr.app_id
    AND value_attr.etype = setting_attr.etype
    AND value_attr.label = CASE setting_attr.etype WHEN 'flags' THEN 'value' ELSE 'toggled' END
  JOIN triples value ON value.app_id = setting.app_id AND value.entity_id = setting.entity_id
    AND value.attr_id = value_attr.id
  WHERE setting_attr.app_id = '24a4d71b-7bb2-4630-9aee-01146af26239'::uuid
    AND setting_attr.label = 'setting'
    AND ((setting_attr.etype = 'flags' AND setting.value = '"magic-code-rate-limit-per-hour"'::jsonb)
      OR (setting_attr.etype = 'toggles' AND setting.value = '"use-bucket4j"'::jsonb))
)
SELECT json_build_object(
  'capacities', COALESCE((SELECT json_agg(value) FROM settings WHERE etype = 'flags'), '[]'::json),
  'durableToggles', COALESCE((SELECT json_agg(value) FROM settings WHERE etype = 'toggles'), '[]'::json),
  'durableTablePresent', to_regclass('rate_limit_keys') IS NOT NULL,
  'durableTableLogged', EXISTS (SELECT 1 FROM pg_class WHERE oid = to_regclass('rate_limit_keys') AND relpersistence = 'p'),
  'databaseReadOnly', current_setting('default_transaction_read_only')::boolean,
  'configApp', json_build_object(
    'adminTokens', (SELECT count(*) FROM app_admin_tokens
                     WHERE app_id = '24a4d71b-7bb2-4630-9aee-01146af26239'::uuid),
    'members', (SELECT count(*) FROM app_members
                 WHERE app_id = '24a4d71b-7bb2-4630-9aee-01146af26239'::uuid)
             + (SELECT count(*) FROM app_member_invites
                 WHERE app_id = '24a4d71b-7bb2-4630-9aee-01146af26239'::uuid),
    'rules', (SELECT code FROM rules
               WHERE app_id = '24a4d71b-7bb2-4630-9aee-01146af26239'::uuid)
  )
);
COMMIT;
