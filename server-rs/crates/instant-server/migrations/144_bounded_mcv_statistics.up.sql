-- Keep high-resolution single-column histograms, but bound the joint MCV list.
-- Inheriting the columns' target of 2500 makes every fresh plan deserialize and
-- examine a large joint list. This changes estimates only, never stored triples.
ALTER STATISTICS triples_attr_value_mcv SET STATISTICS 100;
ANALYZE triples;
