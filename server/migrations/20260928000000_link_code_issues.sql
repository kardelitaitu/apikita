-- =============================================================================
-- Link-code ISSUANCE: the per-account hourly counter.
--
-- WHY THIS TABLE EXISTS, because link_codes cannot do the job
--
-- limits.link_code_issuance_per_hour is documented in docs/server/api-spec.md and
-- docs/decisions.md as bounding how many codes an ACCOUNT may issue in an hour. It
-- enforced that by counting link_codes rows, and 20260926000000 says so explicitly:
-- "the per-ACCOUNT cap counts link_codes rows, which the existing table already
-- carries (account_id, created_at), so it needs nothing new and reuses
-- abuse::enforce_creation_cap unchanged."
--
-- That was true when it was written and stopped being true later. issue_link_code
-- now DELETES every row for the account before inserting the replacement, so that
-- at most one code is live at a time - a real security property, since two live
-- codes double an attacker's chance per guess. The DELETE is what makes the cap
-- unable to count anything: the row count is always 0 or 1, and against the shipped
-- cap of 10 the comparison never fails. The guard parses, runs, and does nothing.
--
-- A later change silently invalidated an earlier documented decision, which is
-- exactly the failure the decision register exists to prevent, and nothing said so
-- because a cap that cannot fire is indistinguishable from a cap that is not being
-- hit. Measured before this table existed: eighteen concurrent issues against a
-- cap of ten produced eighteen successes and zero refusals.
--
-- SO: issuances are recorded here, and the cap counts THIS table. Deliberately NOT
-- by superseding link_codes rows instead of deleting them - that would make the
-- rows accumulate, which does fix the count, but the one-live-code guarantee is
-- expressed by that DELETE and a second mechanism for it would be a second thing
-- to get wrong. A counter table keeps the live-code behaviour exactly as it is.
--
-- PRIVACY: an account_id and a timestamp. No code value, no address, nothing that
-- identifies a person beyond the account already in the request.
--
-- RETENTION: the same class and the same 7-day sweep as key_ip_seen. This is an
-- abuse signal, not customer data, and it is deliberately shaped like
-- key_ip_seen so one sweep in ip_tracking::purge_expired covers it. The period is
-- NOT re-decided here - docs/data-retention.md is the single source.
-- =============================================================================

CREATE TABLE link_code_issues (
  account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
  created_at TEXT NOT NULL CHECK (created_at GLOB '????-??-??T??:??:??*+00:00')
) STRICT;

-- The limiter's only query, exactly as on link_redemption_attempts: count this
-- account's issuances since a lower bound, and MIN(created_at) for the Retry-After.
-- Both are served by this index, so the guard does not become a table scan on the
-- path it exists to protect.
CREATE INDEX link_code_issues_account_idx
  ON link_code_issues (account_id, created_at);
