-- Run `create index concurrently triples_app_id on triples (app_id)` in
-- production first; this single statement also runs outside a transaction.
create index concurrently if not exists triples_app_id on triples (app_id);
