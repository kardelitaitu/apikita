-- CI smoke fixture for the maintenance scheduler.
--
-- The minimum schema the retention sweep touches, plus one row PAST each window
-- and one INSIDE it. The rows are what make the check meaningful: exit 0 alone
-- would pass even if a DELETE silently matched nothing (which is exactly what a
-- bare-date cutoff against an RFC3339 timestamp would do), so the step compares
-- the row count before and after.
--
-- Timestamps are RFC3339, the shape the real schema stores.

DROP TABLE IF EXISTS key_ip_seen;
CREATE TABLE key_ip_seen (day TEXT);
DROP TABLE IF EXISTS key_ip_daily;
CREATE TABLE key_ip_daily (day TEXT);
DROP TABLE IF EXISTS usage_daily;
CREATE TABLE usage_daily (day TEXT);
DROP TABLE IF EXISTS usage_events;
CREATE TABLE usage_events (id TEXT, created_at TEXT);
DROP TABLE IF EXISTS sessions;
CREATE TABLE sessions (id TEXT, expires_at TEXT, revoked_at TEXT);

-- Past the window: these must be deleted.
INSERT INTO usage_events VALUES ('old', '2000-01-01T00:00:00+00:00');
INSERT INTO usage_daily  VALUES ('2000-01-01');
-- Inside the window: these must survive.
INSERT INTO usage_events VALUES ('new', '2099-01-01T00:00:00+00:00');
INSERT INTO usage_daily  VALUES ('2099-01-01');
