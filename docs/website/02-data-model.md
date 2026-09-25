# 02 — Data Model (PostgreSQL)

Schema for everything except identity.

> **Rewritten for the Postgres + PocketBase split.** The earlier version specified
> PocketBase collections and API rules. Identity now lives in PocketBase; money and
> usage live in Postgres. See [`docs/architecture.md`](../architecture.md).

## Division of ownership

| Concern | Store |
| --- | --- |
| Accounts, passwords, Google login, verification, reset, MFA | **PocketBase** |
| Wallet, keys, limits, top-ups, usage, ledger, sessions | **Postgres** |

**The golden rule:** Postgres never becomes authoritative about *who* a user is,
and PocketBase never holds money. Rust reads identity from PocketBase and writes
money to Postgres — never the reverse.

## Extensions

```sql
CREATE EXTENSION IF NOT EXISTS pgcrypto;   -- gen_random_uuid()
```

## accounts

The join point between the two systems.

```sql
CREATE TABLE accounts (
  id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
  pb_user_id  TEXT UNIQUE NOT NULL,        -- PocketBase users.id (auto-indexed by UNIQUE)
  status      TEXT NOT NULL DEFAULT 'active'
              CHECK (status IN ('active','suspended','closed')),
  is_operator BOOLEAN NOT NULL DEFAULT false,   -- operator surface access
  created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
  updated_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);
```

**Why `pb_user_id` and not PocketBase's id as the PK.** Auth is the component most
likely to change; it must not own the primary key of the ledger. The cost is one
extra lookup on login, paid once per session.

**Never hard-delete a PocketBase user.** The wallet is here and nothing cascades
across the boundary — deleting the auth record orphans the money. Set
`status = 'closed'` instead.

## wallets

```sql
CREATE TABLE wallets (
  account_id  UUID PRIMARY KEY REFERENCES accounts(id) ON DELETE RESTRICT,
  balance_idr BIGINT NOT NULL DEFAULT 0 CHECK (balance_idr >= 0),
  updated_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);
```

- **`BIGINT`, never floating point.** Money is integers.
- **`CHECK (balance_idr >= 0)`** — the database refuses to go negative. If the
  design ever needs a small overdraft (letting a stream finish past zero), that is
  a deliberate change to this constraint, not a silent one.
- `ON DELETE RESTRICT`: an account with a wallet cannot be deleted. Closing is a
  status change.
- One wallet per account, so `account_id` is the PK — no separate id needed.

## ledger — append-only

The balance is derivable from this. That is what makes a dispute resolvable.

```sql
CREATE TABLE ledger (
  id            BIGSERIAL PRIMARY KEY,
  account_id    UUID NOT NULL REFERENCES accounts(id) ON DELETE RESTRICT,
  delta_idr     BIGINT NOT NULL,          -- positive = credit, negative = debit
  reason        TEXT NOT NULL
                CHECK (reason IN ('topup','usage','adjustment','refund')),
  ref           TEXT,                      -- topup id, usage batch id, etc.
  balance_after BIGINT NOT NULL,           -- snapshot, for audit
  created_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX ledger_account_created_idx ON ledger (account_id, created_at DESC);
```

**Never UPDATE or DELETE a ledger row.** A correction is a new `adjustment` row
that reverses the error. An append-only ledger is the only version of this that
survives scrutiny.

`balance_after` is denormalised on purpose: it proves what the balance was at a
point in time without replaying history.

## sessions

Server-side sessions — this is what makes logout actually revoke.

```sql
CREATE TABLE sessions (
  id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
  account_id  UUID NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
  token_hash  TEXT NOT NULL UNIQUE,        -- hash of the opaque cookie value
  expires_at  TIMESTAMPTZ NOT NULL,
  revoked_at  TIMESTAMPTZ,
  user_agent  TEXT,
  ip_hash     TEXT,                        -- hashed, not raw IP
  created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX sessions_account_idx ON sessions (account_id) WHERE revoked_at IS NULL;
CREATE INDEX sessions_expires_idx ON sessions (expires_at);
```

- Cookie carries an **opaque random value**; only its hash is stored.
- **Logout revokes the row** → immediate, on every surface.
- "Sign out everywhere" = revoke all rows for the account.
- `CASCADE` is correct here: deleting an account should kill its sessions.
- Sweep expired rows on a schedule; do not let the table grow forever.

## api_keys

The hot path — every proxied request looks up by hash.

```sql
CREATE TABLE api_keys (
  id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
  account_id      UUID NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
  key_hash        TEXT NOT NULL UNIQUE,   -- SHA-256 of the full key
  prefix          TEXT NOT NULL,          -- display, e.g. 'apk_live_a1b2'
  label           TEXT,
  models          JSONB NOT NULL DEFAULT '[]'::jsonb,  -- allowed public names
  spend_limit_idr BIGINT NOT NULL DEFAULT 0,           -- 0 = unlimited
  token_limit     BIGINT NOT NULL DEFAULT 0,
  rate_limit_rpm  INTEGER NOT NULL DEFAULT 0,
  expires_at      TIMESTAMPTZ,
  last_used_at    TIMESTAMPTZ,            -- updated lazily, not per request
  revoked_at      TIMESTAMPTZ,
  created_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX api_keys_hash_idx ON api_keys (key_hash) WHERE revoked_at IS NULL;
CREATE INDEX api_keys_account_idx ON api_keys (account_id);
```

- **`key_hash` is SHA-256, not a password hash.** The key is a high-entropy random
  token, not a guessable password, and this lookup happens on every request. A slow
  KDF here would add latency to every token.
- **`models` empty array = can call nothing.** Deny by default. Never a boolean
  "allow all" — that would silently grant future models to existing keys.
- **`ON DELETE CASCADE`**: deleting an account should remove its keys.
- Do not write `last_used_at` on every request — it turns a read path into a write
  path and will contend under load. Update lazily or in batches.

Full behaviour: [06-api-keys-and-limits.md](06-api-keys-and-limits.md).

## topups

```sql
CREATE TABLE topups (
  id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
  account_id  UUID NOT NULL REFERENCES accounts(id) ON DELETE RESTRICT,
  amount_idr  BIGINT NOT NULL CHECK (amount_idr > 0),
  order_id    TEXT NOT NULL UNIQUE,       -- Midtrans order_id: idempotency key
  status      TEXT NOT NULL DEFAULT 'pending'
              CHECK (status IN ('pending','settled','denied','expired','refunded')),
  snap_token  TEXT,
  created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
  settled_at  TIMESTAMPTZ
);

CREATE INDEX topups_account_created_idx ON topups (account_id, created_at DESC);
```

**`order_id` is UNIQUE** — the database enforces webhook idempotency. A retried
webhook cannot double-credit, because the second write matches an already-settled
row.

See [04-payments.md](04-payments.md) for the full flow.

## usage_daily

Aggregate for the dashboard, the realtime stream, and reconciliation.

```sql
CREATE TABLE usage_daily (
  account_id        UUID NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
  api_key_id        UUID REFERENCES api_keys(id) ON DELETE SET NULL,
  day               DATE NOT NULL,
  input_tokens      BIGINT NOT NULL DEFAULT 0,
  cache_read_tokens BIGINT NOT NULL DEFAULT 0,
  output_tokens     BIGINT NOT NULL DEFAULT 0,
  cost_idr          BIGINT NOT NULL DEFAULT 0,
  PRIMARY KEY (account_id, api_key_id, day)
);

CREATE INDEX usage_daily_account_day_idx ON usage_daily (account_id, day DESC);
```

**Three separate token counters, always.** Cache-read tokens are priced ~50x below
input and ~200x below output. Collapsing them into one "tokens" column makes both
the spend limit and the invoice impossible to reconcile.

Keyed by `(account_id, api_key_id, day)` so a per-key spend limit is computable
without scanning raw events.

## link_codes

Telegram binding.

```sql
CREATE TABLE link_codes (
  code       TEXT PRIMARY KEY,            -- 6 digits
  account_id UUID NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
  expires_at TIMESTAMPTZ NOT NULL,
  used_at    TIMESTAMPTZ,
  created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
```

**Redemption must be rate-limited.** A 6-digit code is brute-forceable, and a
successful guess attaches an attacker's Telegram to a funded wallet. Cap attempts
per account and per IP; invalidate on use; invalidate the previous code when a new
one is issued.

## telegram_links

```sql
CREATE TABLE telegram_links (
  telegram_id TEXT PRIMARY KEY,           -- one Telegram -> one account
  account_id  UUID NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
  linked_at   TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX telegram_links_account_idx ON telegram_links (account_id);
```

## reviews

Customer reviews. **Written from Telegram only** — never from the website. One per
user, editable.

```sql
CREATE TABLE reviews (
  id           UUID PRIMARY KEY DEFAULT gen_random_uuid(),
  account_id   UUID REFERENCES accounts(id) ON DELETE SET NULL,
  telegram_id  TEXT,                   -- set before linking; kept for audit
  rating       SMALLINT NOT NULL CHECK (rating BETWEEN 1 AND 5),
  body         TEXT CHECK (length(body) <= 1000),
  is_customer  BOOLEAN NOT NULL DEFAULT false,  -- had a settled top-up at write time
  withdrawn_at TIMESTAMPTZ,            -- set instead of deleting the row
  created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
  updated_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
  -- a row must be attributable to someone
  CHECK (account_id IS NOT NULL OR telegram_id IS NOT NULL)
);

-- one review per account once linked
CREATE UNIQUE INDEX reviews_account_uniq ON reviews (account_id)
  WHERE account_id IS NOT NULL;
-- one review per telegram id before linking
CREATE UNIQUE INDEX reviews_telegram_uniq ON reviews (telegram_id)
  WHERE account_id IS NULL;
```

**Keyed on the account, not the Telegram id.** A review submitted before linking
is held against `telegram_id` and re-attributed on link. Without this, linking a
Telegram account to a web account creates a **second review slot** and the
"one review per user" rule silently breaks.

**The two partial indexes do NOT by themselves prevent a double review.** They
sit on *different columns*, so they cannot see a row keyed by `telegram_id`
and a row keyed by `account_id` that belong to the same person. The gap:

| Step | State |
| --- | --- |
| 1. User reviews pre-link | row: `telegram_id=T, account_id=NULL` |
| 2. User links account `A` | re-attribution must run **now**, atomically |
| 3. If it does not, and the user later submits via the bot with a resolved account | second row: `account_id=A` |

**Rules that close it:**

1. **Re-attribute at link time, in the same transaction that binds the Telegram
   account.** Not lazily, not on a schedule.
2. **All review writes resolve to `account_id` first**, falling back to
   `telegram_id` only when no account is linked. Writes arrive from the bot
   only, so `telegram_id` is always present.
3. The re-attribution sets `account_id`; if a row already exists for that
   account, **merge rather than duplicate** (keep the newer, log the collision).

**Withdrawal is a flag, not a row delete.** `withdrawn_at` is marked and the row
kept. A deleted row would free the unique slot and let the user submit again,
breaking "one review per user" — and destroying the history that makes an edit
meaningful.

**`ON DELETE SET NULL`, not CASCADE.** A closed account should not erase its
review — the review is the operator's record, and a customer's history is the
point of collecting it.

### review_history

Edits must be auditable. An edited review that drops from 5 stars to 1 is a
signal; overwriting it destroys the signal.

```sql
CREATE TABLE review_history (
  id          BIGSERIAL PRIMARY KEY,
  review_id   UUID NOT NULL REFERENCES reviews(id) ON DELETE CASCADE,
  rating      SMALLINT NOT NULL,
  body        TEXT,
  replaced_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
```

**Never delete a review silently.** If one must be hidden, record that it was.

See [`docs/telegram/README.md`](../telegram/README.md) for the bot flow.

## admin_audit

Every operator action. See [`admin-surface.md`](../admin-surface.md).

```sql
CREATE TABLE admin_audit (
  id           BIGSERIAL PRIMARY KEY,
  operator_id  UUID NOT NULL REFERENCES accounts(id) ON DELETE RESTRICT,
  action       TEXT NOT NULL,          -- 'suspend', 'adjust', 'refund', ...
  target_type  TEXT NOT NULL,          -- 'account', 'key', 'topup', 'review'
  target_id    TEXT NOT NULL,
  detail       JSONB,                  -- before/after, amount, note
  created_at   TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX admin_audit_operator_idx ON admin_audit (operator_id, created_at DESC);
CREATE INDEX admin_audit_target_idx   ON admin_audit (target_type, target_id);
```

**`ON DELETE RESTRICT` on the operator** — the audit trail outlives the operator's
account. Deleting an operator must not erase what they did.

**Written in the same transaction as the effect it records.** An audit row for an
action that rolled back is as misleading as no audit row at all.

## key_ip_daily / key_ip_seen

Abuse signals without storing IP addresses. Full reasoning:
[`docs/ip-tracking.md`](../ip-tracking.md).

```sql
CREATE TABLE key_ip_daily (
  api_key_id    UUID NOT NULL REFERENCES api_keys(id) ON DELETE CASCADE,
  day           DATE NOT NULL,
  distinct_ips  INTEGER NOT NULL DEFAULT 0,
  request_count BIGINT NOT NULL DEFAULT 0,
  PRIMARY KEY (api_key_id, day)
);

CREATE TABLE key_ip_seen (
  api_key_id UUID NOT NULL REFERENCES api_keys(id) ON DELETE CASCADE,
  day        DATE NOT NULL,
  ip_hash    TEXT NOT NULL,          -- HMAC of a daily salt; salt is deleted daily
  PRIMARY KEY (api_key_id, day, ip_hash)
);
```

**No raw IP is stored anywhere.** `ip_hash` is an HMAC against a salt that is
deleted each day, so the hashes become permanently unlinkable. `key_ip_seen` is
retained 7 days; the daily aggregate 90 days.

**Do not carry this pattern into anything that builds a per-user history.** See
[`docs/ip-tracking.md`](../ip-tracking.md) for what is deliberately not built.

## Transactions — the correctness rules

These operations must be atomic. Getting one wrong is how balances drift.

### Credit a top-up

```sql
BEGIN;
  SELECT * FROM topups WHERE order_id = $1 FOR UPDATE;   -- lock the row
  -- if status = 'settled': COMMIT and return (idempotent, do nothing)
  UPDATE topups SET status='settled', settled_at=now() WHERE id=$2;
  UPDATE wallets SET balance_idr = balance_idr + $3, updated_at=now()
    WHERE account_id=$4
    RETURNING balance_idr;                                -- capture for ledger
  INSERT INTO ledger (account_id, delta_idr, reason, ref, balance_after)
    VALUES ($4, $3, 'topup', $2, $5);
COMMIT;
```

`FOR UPDATE` on the topup row is what makes concurrent webhook retries safe.

### Debit usage

Same shape: update `wallets`, insert a `ledger` row with a negative delta, upsert
`usage_daily`. All in one transaction, or a crash leaves usage charged but not
recorded — or the reverse.

## review_sessions

Transient state for the Telegram review conversation. See
[`docs/telegram/README.md`](../telegram/README.md) for the flow.

```sql
CREATE TABLE review_sessions (
  telegram_id TEXT PRIMARY KEY,
  step        TEXT NOT NULL,         -- 'awaiting_rating' | 'awaiting_body'
  rating      SMALLINT,              -- staged, not yet written to reviews
  body        TEXT,
  editing     BOOLEAN NOT NULL DEFAULT false,
  expires_at  TIMESTAMPTZ NOT NULL
);
```

**Staged values are not the review.** Nothing is written to `reviews` until the
flow completes, so abandoning halfway leaves an existing review untouched.

**Could live in Redis instead.** It is transient and does not need durability. Postgres is specified because the stack already has it and the row count is tiny;
move it to Redis only if the write volume justifies another dependency.

Sweep expired rows on a schedule.

## Index coverage

**Every documented lookup is covered.** Verified by mapping each hot-path query to
its index:

| Lookup | Covered by |
| --- | --- |
| Proxy key auth — `api_keys.key_hash` | explicit partial index |
| Webhook idempotency — `topups.order_id` | `UNIQUE` (auto-indexed) |
| Session resolve — `sessions.token_hash` | `UNIQUE` (auto-indexed) |
| Login — `accounts.pb_user_id` | `UNIQUE` (auto-indexed) |
| Dashboard usage — `usage_daily.account_id` | composite PK (leftmost) |
| Keys per account — `api_keys.account_id` | explicit index |
| Telegram resolve — `telegram_links.telegram_id` | `PRIMARY KEY` (auto-indexed) |
| Link redeem — `link_codes.code` | `PRIMARY KEY` (auto-indexed) |
| Review by user — `reviews.account_id` | partial unique index |
| Ledger view — `ledger.account_id, created_at` | composite index |
| Abuse signal — `key_ip_seen.api_key_id` | composite PK (leftmost) |
| Audit view — `admin_audit.operator_id` | composite index |

### Postgres indexes PK and UNIQUE columns automatically

**Do not add an explicit index for a column that is already a `PRIMARY KEY` or
carries a `UNIQUE` constraint.** It is redundant: PostgreSQL creates a unique
B-tree index for both, and a duplicate costs write throughput and storage for no
read benefit.

That covers `order_id`, `token_hash`, `telegram_id`, `code`, and
`pb_user_id` in this schema — each is constrained, each already indexed.

**A composite primary key is only useful leftmost-first.** `usage_daily` is keyed
`(`account_id, api_key_id, day`)`, so a query filtering on `day` alone cannot
use it. No such query exists today; if one is added, it needs its own index.

### The rule

**Every hot-path query needs a covering index, and a hot-path query is one that runs
per request — not per report.** The proxy's key lookup is the one that matters most:
it runs on every token, so it is a partial index on `key_hash` filtered to
not-revoked rows.

## Backups

The wallet ledger is the business.

- **PITR, or at minimum daily snapshots**, retained off-host.
- **Verify restores.** An untested backup is a belief, not a backup.
- PocketBase needs backing up too — losing it loses logins, though not money.
- **`ledger` is the authoritative record.** If `wallets.balance_idr` and the sum of
  `ledger.delta_idr` ever disagree, the ledger is right and the balance is a bug to
  investigate.

## Schema rules, collected

1. Money is `BIGINT` IDR. Never float.
2. `ledger` is append-only. Corrections are new rows.
3. `order_id` unique — webhook idempotency enforced by the database.
4. `api_keys` looked up by `key_hash`; indexed, partial on not-revoked.
5. Wallet mutations are transactional with their ledger row.
6. `ON DELETE RESTRICT` for anything holding money; `CASCADE` only for derived
   data (sessions, keys, usage).
7. Never hard-delete a PocketBase user.
8. Reconcile `accounts.pb_user_id` against PocketBase on a schedule.
9. `reviews` is keyed on the account; one per user, enforced by a partial
   unique index, and edits are appended to `review_history`.
10. **Admin actions write `admin_audit`, and money moves only via `ledger`** —
    never a direct balance edit.
11. **This document is the only place tables are defined.** Other docs reference it;
    a second copy of the DDL drifted once already (missing `is_operator`).

## Open items

- [x] Migration tooling: **sqlx migrate** — [`docs/decisions.md`](../decisions.md).
- [x] Session lifetime: **30d absolute / 7d idle** — [`docs/decisions.md`](../decisions.md).
- [ ] Whether to keep raw usage events alongside `usage_daily`.
- [ ] Retention for `sessions` and `ledger` (ledger: keep forever).
- [ ] Reconciliation job: schedule and where alerts go.