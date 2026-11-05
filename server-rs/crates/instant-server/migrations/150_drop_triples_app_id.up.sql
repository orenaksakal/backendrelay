-- triples_app_id duplicates the leading column of the primary key
-- (app_id, entity_id, attr_id, value_md5), so every app-scoped query can use
-- triples_pkey instead. Run `drop index concurrently triples_app_id` in
-- production first; this single statement also runs outside a transaction.
drop index concurrently if exists triples_app_id;
