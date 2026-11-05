-- The locked config app is intentionally retained on rollback. Deleting it
-- would cascade into every operator flag stored in it, and an older binary
-- works fine with the locked config app present.
select 1;
