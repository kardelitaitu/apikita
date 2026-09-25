-- Ledger reconciliation query (apikita launch Gate 2).
-- Verbatim from docs/observability.md, with real table/column names confirmed
-- against server/migrations/20260925000000_initial_schema.sql.
--
-- Invariant: wallets.balance_idr must equal the sum of the ledger deltas for
-- the same account. The ledger is authoritative; wallets is a cache of it.
-- Any row returned here means money is wrong and must be investigated before
-- a customer notices.

SELECT w.account_id, w.balance_idr, COALESCE(SUM(l.delta_idr), 0) AS ledger_sum
FROM wallets w
LEFT JOIN ledger l ON l.account_id = w.account_id
GROUP BY w.account_id, w.balance_idr
HAVING w.balance_idr <> COALESCE(SUM(l.delta_idr), 0);
