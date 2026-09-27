-- =============================================================================
-- Link-code redemption: the per-IP attempt counter.
--
-- WHY A NEW TABLE RATHER THAN A COLUMN ON link_codes
--
-- The per-ACCOUNT cap counts link_codes rows, which the existing table already
-- carries (account_id, created_at), so it needs nothing new and reuses
-- abuse::enforce_creation_cap unchanged.
--
-- The per-IP cap counts SOMETHING ELSE: attempts from one client, whether or not
-- a code existed and whether or not the account was involved. A malformed or
-- entirely unknown code has no link_codes row to hang off, so the counter cannot
-- live on that table. It is also the only thing that makes the endpoint safe -
-- a 6-digit code is 10^6 possibilities and the refusal never says whether the
-- code existed (docs/architecture/identity.md, "the highest-risk endpoint in the
-- Telegram surface") - so it must count FAILURES, which by definition may have no
-- code behind them.
--
-- PRIVACY: ip_hash is the salted HMAC from ip_tracking::ip_hash. The raw address
-- is never stored, per docs/data-retention.md and Gate 4 of the launch checklist
-- ("No raw IP address is persisted; only a salted hash").
--
-- RETENTION: this is an abuse signal, not customer data. purge_expired() in
-- ip_tracking.rs owns the sweep for the key_ip_* tables; these rows are the same
-- class and are intended to age out on the same schedule. The retention period is
-- NOT re-decided here - docs/data-retention.md is the single source for periods,
-- and this table is deliberately shaped like key_ip_seen so one sweep can cover
-- both without a second policy.
-- =============================================================================

CREATE TABLE link_redemption_attempts (
  ip_hash      TEXT NOT NULL,
  attempted_at TEXT NOT NULL CHECK (attempted_at GLOB '????-??-??T??:??:??*+00:00')
) STRICT;

-- The limiter's only query: count this ip_hash's attempts since a lower bound,
-- and take MIN(attempted_at) for the Retry-After. Both are served by this index,
-- and it is what keeps the guard from becoming a table scan on the attack path.
CREATE INDEX link_redemption_attempts_ip_idx
  ON link_redemption_attempts (ip_hash, attempted_at);
