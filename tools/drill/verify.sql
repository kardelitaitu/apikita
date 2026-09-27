-- Restore-drill metrics (apikita) - SQLite.
-- The "Verify the numbers that matter" step of docs/backup-and-restore.md
-- ("The restore drill", step 3), plus the two row-count pass criteria.
--
-- Ported from PostgreSQL: every `count(*)::text` became `CAST(count(*) AS TEXT)`
-- and every `sum(x)::text` became `CAST(COALESCE(sum(x), 0) AS TEXT)`. The sqlite3
-- CLI rejects `::` with "unrecognized token: ':'", so an unported query fails the
-- whole drill with exit 4 rather than reporting a metric - loud, not silent.
-- COALESCE is explicit for sum(): SQLite's sum() over zero rows is NULL, exactly
-- as in Postgres, and the drill's metric parser needs a value on every line.
--
-- This file deliberately contains NO wallet/ledger drift query. Drift is defined
-- in exactly one place in this repository - tools/reconcile/reconcile.sql - and
-- drill.sh runs THAT definition (see README.md, "The check"). A second copy here
-- is how a detector stops being trusted.
--
-- Output: one "metric|value" line per row, unaligned and unheadered
-- (sqlite3 -noheader -separator '|'), so the shell can parse it without guessing.

SELECT 'accounts' AS metric, CAST(count(*) AS TEXT) AS value FROM accounts
UNION ALL SELECT 'wallets', CAST(count(*) AS TEXT) FROM wallets
UNION ALL SELECT 'ledger', CAST(count(*) AS TEXT) FROM ledger
UNION ALL SELECT 'api_keys', CAST(count(*) AS TEXT) FROM api_keys
UNION ALL SELECT 'topups', CAST(count(*) AS TEXT) FROM topups
UNION ALL SELECT 'usage_daily', CAST(count(*) AS TEXT) FROM usage_daily
-- docs/backup-and-restore.md pass criteria: "Keys present - key_hash rows exist
-- (auth would still work)". A restore with accounts but no key hashes is a
-- restore where nobody can authenticate.
UNION ALL SELECT 'api_keys_with_key_hash', CAST(count(*) AS TEXT) FROM api_keys
     WHERE key_hash IS NOT NULL AND key_hash <> ''
-- Step 3's money totals. The ledger is authoritative; wallets is a cache, so the
-- two must agree globally. This is strictly WEAKER than the per-account check
-- (per-account drift can cancel out in a global sum), which is exactly why the
-- gate is reconcile.sql and this is only a sanity total.
UNION ALL SELECT 'total_owed_idr', CAST(COALESCE(sum(balance_idr), 0) AS TEXT) FROM wallets
UNION ALL SELECT 'ledger_sum_idr', CAST(COALESCE(sum(delta_idr), 0) AS TEXT) FROM ledger
ORDER BY metric;
