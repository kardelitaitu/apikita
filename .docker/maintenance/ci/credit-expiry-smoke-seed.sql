-- Seeds for the credit-expiry smoke: ONE settled deposit whose credit has ALREADY EXPIRED,
-- balanced by its own funding ledger row.
--
-- WHY THE FUNDING ROW IS HERE, and it is the part worth keeping. The check that follows
-- compares the wallet total against SUM(ledger.delta_idr) - the Gate 2 invariant - so the
-- fixture has to START balanced or the comparison proves nothing about the sweep. A wallet
-- with 50000 and no ledger row is +50000 that the ledger cannot account for; the sweep would
-- then move the balance and the totals would still disagree, and the disagreement would be
-- the FIXTURE's rather than the code's. MEASURED while wiring this by hand: wallets=0
-- ledger=-50000, which reads like a broken invariant and was a broken seed.
--
-- THE ACCOUNT ID IS A UUID, and the first version of this file got that wrong. It used
-- `acct-expired`, in the style of hold-sweep-smoke-seed.sql - but that fixture is only ever
-- read by SQL comparisons, whereas this one reaches `db::expire_credit`, which parses
-- `account_id` as a `Uuid`. MEASURED: `called Result::unwrap() on an Err value: ColumnDecode
-- { index: "account_id", source: ParseChar { character: 't', index: 3 } }` - a panic, because
-- `acct-` is not a uuid. A fixture that panics the code under test proves nothing about it,
-- and the failure looks like a bug in the sweep rather than in the seed.
--
-- credit_expires_at is in the past so the sweep has something to retire, and well before the
-- run, which makes the test deterministic rather than dependent on clock skew.

INSERT INTO accounts (id, is_operator, created_at, updated_at)
VALUES ('550e8400-e29b-41d4-a716-446655440000', 0, '2020-01-01T00:00:00+00:00', '2020-01-01T00:00:00+00:00');

INSERT INTO wallets (account_id, balance_idr, updated_at)
VALUES ('550e8400-e29b-41d4-a716-446655440000', 50000, '2020-01-01T00:00:00+00:00');

-- The deposit's own credit, exactly as settlement writes it: a positive ledger row under the
-- bare topup id, and a wallet that reflects it. The sweep's expiry row is filed under
-- `expiry:<id>` so the two are distinguishable.
INSERT INTO ledger (account_id, delta_idr, reason, ref, balance_after, created_at)
VALUES ('550e8400-e29b-41d4-a716-446655440000', 50000, 'topup', 'topup-expired-w35', 50000, '2020-01-01T00:00:00+00:00');

INSERT INTO topups (id, account_id, amount_idr, order_id, rail, status, created_at, settled_at,
                    credit_expires_at)
VALUES ('topup-expired-w35', '550e8400-e29b-41d4-a716-446655440000', 50000, 'order-expired-w35',
        'midtrans', 'settled',
        '2020-01-01T00:00:00+00:00', '2020-01-01T00:00:00+00:00', '2021-01-01T00:00:00+00:00');
