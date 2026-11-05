-- Restore the inherited target used before this migration and rebuild its list.
ALTER STATISTICS triples_attr_value_mcv SET STATISTICS DEFAULT;
ANALYZE triples;
