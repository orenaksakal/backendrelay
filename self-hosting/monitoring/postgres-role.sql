-- Run on each database as its administrator. Set a distinct password through
-- psql's \password backendrelay_monitor (never commit one in this file).
CREATE ROLE backendrelay_monitor LOGIN;
GRANT pg_monitor TO backendrelay_monitor;
-- Primary database: also execute postgres-primary-grants.sql after migrations.
