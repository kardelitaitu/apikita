-- Restore-drill metrics (apikita).
-- The "Verify the numbers that matter" step of docs/backup-and-restore.md
-- ("The restore drill", step 3), plus the two row-count pass criteria.
--
-- This file deliberately contains NO wallet/ledger drift query. Drift is defined
-- in exactly one place in this repository - tools/reconcile/reconcile.sql - and
-- drill.sh runs THAT definition (see README.md, "The check"). A second copy here
-- is how a detector stops being trusted.
--
-- Output: one "metric|value" line per row, unaligned (-t -A -F'|'), so the shell
-- can parse it without guessing.

SELECT 'accounts' AS metric, count(*)::text AS value FROM accounts
UNION ALL SELECT 'wallets', count(*)::text FROM wallets
UNION ALL SELECT 'ledger', count(*)::text FROM ledger
UNION ALL SELECT 'api_keys', count(*)::text FROM api_keys
UNION ALL SELECT 'topups', count(*)::text FROM topups
UNION ALL SELECT 'usage_daily', count(*)::text FROM usage_daily
-- docs/backup-and-restore.md pass criteria: "Keys present - key_hash rows exist
-- (auth would still work)". A restore with accounts but no key hashes is a
-- restore where nobody can authenticate.
UNION ALL SELECT 'api_keys_with_key_hash', count(*)::text FROM api_keys
     WHERE key_hash IS NOT NULL AND key_hash <> ''
-- Step 3's money totals. The ledger is authoritative; wallets is a cache, so the
-- two must agree globally. This is strictly WEAKER than the per-account check
-- (per-account drift can cancel out in a global sum), which is exactly why the
-- gate is reconcile.sql and this is only a sanity total.
UNION ALL SELECT 'total_owed_idr', COALESCE(sum(balance_idr), 0)::text FROM wallets
UNION ALL SELECT 'ledger_sum_idr', COALESCE(sum(delta_idr), 0)::text FROM ledger
ORDER BY metric;
