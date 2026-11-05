-- compat: additive
-- triples is the hottest, most-updated table; the default 20% scale factor
-- lets dead tuples and stale statistics accumulate between autovacuum runs.
alter table triples set (
  autovacuum_vacuum_scale_factor = 0.02,
  autovacuum_analyze_scale_factor = 0.01
);
