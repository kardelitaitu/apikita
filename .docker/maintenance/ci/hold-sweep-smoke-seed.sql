-- Seeds for the hold-sweep smoke: ONE stranded hold and ONE healthy reservation.
-- The distinction is the whole test: a detector that fires on both is noise, one
-- that fires on neither is absent.

INSERT INTO accounts (id, pb_user_id, is_operator, created_at, updated_at)
VALUES ('acct-stranded', 'pb_stranded', 0, '2026-01-01T00:00:00+00:00', '2026-01-01T00:00:00+00:00');
INSERT INTO wallets (account_id, balance_idr, updated_at)
VALUES ('acct-stranded', 40000, '2026-01-01T00:00:00+00:00');

INSERT INTO accounts (id, pb_user_id, is_operator, created_at, updated_at)
VALUES ('acct-healthy', 'pb_healthy', 0, '2026-01-01T00:00:00+00:00', '2026-01-01T00:00:00+00:00');
INSERT INTO wallets (account_id, balance_idr, updated_at)
VALUES ('acct-healthy', 60000, '2026-01-01T00:00:00+00:00');

-- The STRANDED hold: a negative reserve_ row, an hour old, with NO matching positive.
INSERT INTO ledger (account_id, delta_idr, reason, ref, balance_after, created_at)
VALUES ('acct-stranded', -10000, 'usage', 'reserve_stranded_w34', 40000, '2020-01-01T00:00:00+00:00');

-- The HEALTHY reservation: negative AND a matching positive under the same ref.
INSERT INTO ledger (account_id, delta_idr, reason, ref, balance_after, created_at)
VALUES ('acct-healthy', -20000, 'usage', 'reserve_healthy_w34', 60000, '2020-01-01T00:00:00+00:00');
INSERT INTO ledger (account_id, delta_idr, reason, ref, balance_after, created_at)
VALUES ('acct-healthy', 20000, 'adjustment', 'reserve_healthy_w34', 60000, '2020-01-01T00:00:01+00:00');