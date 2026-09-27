-- Ledger reconciliation query (apikita launch Gate 2).
-- Derived from docs/observability.md, with real table/column names confirmed
-- against server/migrations/20260925000000_initial_schema.sql.
--
-- Invariant: wallets.balance_idr must equal the sum of the ledger deltas for the
-- same account. The ledger is AUTHORITATIVE; wallets is a cache of it. Any row
-- returned here means money is wrong and must be investigated before a customer
-- notices.
--
-- FULL OUTER JOIN, deliberately - not LEFT JOIN. Anchoring on wallets cannot see
-- an account that has ledger money but NO wallets row: the cache is missing
-- entirely, which is the worst case, not an ignorable one. A +250000 adjustment
-- with no wallet row used to return nothing and the gate reported PASSING.
-- Both directions now return a row:
--   * wallets row disagrees with its ledger sum -> balance_idr vs ledger_sum
--   * ledger rows with NO wallets row           -> balance_idr = 'NO WALLET ROW'
-- A ledger-only account is reported even when its ledger sums to zero: the cache
-- row itself is missing, so there is no cached figure that agrees with anything.
--
-- balance_idr is rendered as text so a missing wallet row is legible in the
-- unaligned (-t -A) output instead of an empty field. Every returned row carries
-- the account id and both figures: account_id | balance_idr | ledger_sum.
--
-- This query is structurally BLIND to a stranded reservation hold: the debit and
-- its missing offset are both absent from the sum, so the wallet still equals the
-- sum and no row returns. That needs the hold sweep - see README.md and the
-- directive reconcile.sh prints on every run.

SELECT COALESCE(w.account_id, l.account_id) AS account_id,
       CASE WHEN w.account_id IS NULL THEN 'NO WALLET ROW'
            ELSE CAST(w.balance_idr AS TEXT)
       END AS balance_idr,
       CAST(COALESCE(SUM(l.delta_idr), 0) AS TEXT) AS ledger_sum
FROM wallets w
FULL OUTER JOIN ledger l ON l.account_id = w.account_id
GROUP BY w.account_id, l.account_id, w.balance_idr
HAVING w.account_id IS NULL
    OR w.balance_idr <> COALESCE(SUM(l.delta_idr), 0);
