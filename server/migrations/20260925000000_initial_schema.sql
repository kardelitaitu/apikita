-- =============================================================================
-- apikita — SQLite schema (replaces the PostgreSQL initial schema)
--
-- PREREQUISITES
--   1. SQLite >= 3.37.0  — every table below is STRICT, which is what makes
--      "money is INTEGER" enforced rather than merely declared.
--   2. PRAGMA foreign_keys = ON on EVERY connection. Without it every ON DELETE
--      clause below is silently inert and the RESTRICT backstop is gone.
--   3. Every timestamp is bound from Rust. Never write time in SQL: SQLite's
--      CURRENT_TIMESTAMP emits a format that does not compare against RFC3339,
--      which silently extends session lifetimes (see the plan, section 4.6).
-- =============================================================================

-- ---------------------------------------------------------------------------
-- Accounts and identity
-- ---------------------------------------------------------------------------

CREATE TABLE accounts (
  id          TEXT PRIMARY KEY,
  -- PocketBase link. STILL PRESENT and NOT NULL, because PocketBase remains the
  -- identity provider through Phases 1-5: auth.rs creates the account with
  -- `INSERT INTO accounts (pb_user_id) ... ON CONFLICT (pb_user_id)` and
  -- account.rs reads it back. Dropping it here would break login, contradicting
  -- the plan's claim that Phases 1-5 ship with identity intact. Phase 6 drops it.
  pb_user_id  TEXT NOT NULL UNIQUE,
  status      TEXT NOT NULL DEFAULT 'active'
              CHECK (status IN ('active','suspended','closed')),
  is_operator INTEGER NOT NULL DEFAULT 0 CHECK (is_operator IN (0,1)),
  created_at  TEXT NOT NULL CHECK (created_at GLOB '????-??-??T??:??:??*+00:00'),
  updated_at  TEXT NOT NULL CHECK (updated_at GLOB '????-??-??T??:??:??*+00:00')
) STRICT;

-- Replaces PocketBase. One account, many identities (identity.md invariant 1).
-- Additive for now: created empty in Phase 2 and populated in Phase 6, which is
-- when `accounts.pb_user_id` is dropped and accounts.id becomes the only key.
CREATE TABLE identities (
  id             TEXT PRIMARY KEY,
  account_id     TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
  provider       TEXT NOT NULL CHECK (provider IN ('google','password')),
  subject        TEXT NOT NULL,   -- provider's stable subject id
  email          TEXT NOT NULL,
  email_verified INTEGER NOT NULL DEFAULT 0 CHECK (email_verified IN (0,1)),
  password_hash  TEXT,            -- Argon2id; NULL for OAuth-only identities
  created_at     TEXT NOT NULL CHECK (created_at GLOB '????-??-??T??:??:??*+00:00'),
  updated_at     TEXT NOT NULL CHECK (updated_at GLOB '????-??-??T??:??:??*+00:00'),

  UNIQUE (provider, subject),

  -- A password identity must carry a hash; an OAuth identity must not.
  CHECK ((provider = 'password') = (password_hash IS NOT NULL)),

  -- Google guarantees a verified address (identity.md:100). Encode that so an
  -- unverified Google identity cannot exist, rather than trusting the callback.
  CHECK (provider <> 'google' OR email_verified = 1)
) STRICT;

CREATE INDEX identities_account_idx ON identities (account_id);
CREATE UNIQUE INDEX identities_provider_email_uniq ON identities (provider, email);

-- ---------------------------------------------------------------------------
-- Money
-- ---------------------------------------------------------------------------

CREATE TABLE wallets (
  account_id  TEXT PRIMARY KEY REFERENCES accounts(id) ON DELETE RESTRICT,
  balance_idr INTEGER NOT NULL DEFAULT 0 CHECK (balance_idr >= 0),
  updated_at  TEXT NOT NULL CHECK (updated_at GLOB '????-??-??T??:??:??*+00:00')
) STRICT;

-- Append-only. Never UPDATE, never DELETE. Corrections are new rows.
CREATE TABLE ledger (
  id            INTEGER PRIMARY KEY,
  account_id    TEXT NOT NULL REFERENCES accounts(id) ON DELETE RESTRICT,
  delta_idr     INTEGER NOT NULL,
  reason        TEXT NOT NULL
                CHECK (reason IN ('topup','usage','adjustment','refund')),
  ref           TEXT,
  balance_after INTEGER NOT NULL,
  created_at    TEXT NOT NULL CHECK (created_at GLOB '????-??-??T??:??:??*+00:00')
) STRICT;

CREATE INDEX ledger_account_created_idx ON ledger (account_id, created_at DESC);

-- ---------------------------------------------------------------------------
-- Sessions
-- ---------------------------------------------------------------------------

CREATE TABLE sessions (
  id           TEXT PRIMARY KEY,
  account_id   TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
  token_hash   TEXT NOT NULL UNIQUE,
  expires_at   TEXT NOT NULL CHECK (expires_at GLOB '????-??-??T??:??:??*+00:00'),
  -- NEW. The register specifies "30d absolute, 7d idle" but the Postgres schema
  -- had no column for the idle bound, so only the absolute half was enforceable
  -- (auth.rs:255 notes this). Adding it now, while the schema is being rewritten.
  last_seen_at TEXT NOT NULL CHECK (last_seen_at GLOB '????-??-??T??:??:??*+00:00'),
  revoked_at   TEXT CHECK (revoked_at IS NULL
                           OR revoked_at GLOB '????-??-??T??:??:??*+00:00'),
  user_agent   TEXT,
  ip_hash      TEXT,
  created_at   TEXT NOT NULL CHECK (created_at GLOB '????-??-??T??:??:??*+00:00')
) STRICT;

CREATE INDEX sessions_account_idx ON sessions (account_id) WHERE revoked_at IS NULL;
CREATE INDEX sessions_expires_idx ON sessions (expires_at);

-- ---------------------------------------------------------------------------
-- API keys
-- ---------------------------------------------------------------------------

CREATE TABLE api_keys (
  id              TEXT PRIMARY KEY,
  account_id      TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
  key_hash        TEXT NOT NULL UNIQUE,
  prefix          TEXT NOT NULL,
  label           TEXT,
  models          TEXT NOT NULL DEFAULT '[]',
  spend_limit_idr INTEGER NOT NULL DEFAULT 0,
  token_limit     INTEGER NOT NULL DEFAULT 0,
  rate_limit_rpm  INTEGER NOT NULL DEFAULT 0,
  expires_at      TEXT CHECK (expires_at IS NULL
                              OR expires_at GLOB '????-??-??T??:??:??*+00:00'),
  last_used_at    TEXT CHECK (last_used_at IS NULL
                              OR last_used_at GLOB '????-??-??T??:??:??*+00:00'),
  revoked_at      TEXT CHECK (revoked_at IS NULL
                              OR revoked_at GLOB '????-??-??T??:??:??*+00:00'),
  created_at      TEXT NOT NULL CHECK (created_at GLOB '????-??-??T??:??:??*+00:00')
) STRICT;

CREATE INDEX api_keys_hash_idx ON api_keys (key_hash) WHERE revoked_at IS NULL;
CREATE INDEX api_keys_account_idx ON api_keys (account_id);

-- ---------------------------------------------------------------------------
-- Top-ups
-- ---------------------------------------------------------------------------

CREATE TABLE topups (
  id          TEXT PRIMARY KEY,
  account_id  TEXT NOT NULL REFERENCES accounts(id) ON DELETE RESTRICT,
  amount_idr  INTEGER NOT NULL CHECK (amount_idr > 0),
  order_id    TEXT NOT NULL UNIQUE,     -- idempotency, enforced by the DB
  status      TEXT NOT NULL DEFAULT 'pending'
              CHECK (status IN ('pending','settled','denied','expired','refunded')),
  snap_token  TEXT,
  -- Which payment rail funded this top-up. It decides the payout method at
  -- wind-down: a Midtrans customer is paid back by bank transfer, anyone else in
  -- USD stablecoin. Deliberately NOT NULL with NO DEFAULT -- a default of
  -- 'midtrans' would silently mislabel a row written by a path that forgot to
  -- name its rail, whereas this fails loudly. The value set is FROZEN: SQLite
  -- has no ALTER TABLE ... ADD CONSTRAINT, so widening a CHECK means a 12-step
  -- table rebuild. Only the two rails that exist may appear here.
  rail        TEXT NOT NULL CHECK (rail IN ('midtrans','crypto')),
  created_at  TEXT NOT NULL CHECK (created_at GLOB '????-??-??T??:??:??*+00:00'),
  settled_at  TEXT CHECK (settled_at IS NULL
                          OR settled_at GLOB '????-??-??T??:??:??*+00:00')
) STRICT;

CREATE INDEX topups_account_created_idx ON topups (account_id, created_at DESC);

-- ---------------------------------------------------------------------------
-- Usage
-- ---------------------------------------------------------------------------

-- NO PRIMARY KEY. A COALESCE expression cannot appear in a PK, and the
-- NULL-key duplication fix requires it (section 4.3, trap 3). The unique index
-- below IS the key, and the upsert must target it by name.
--
-- api_key_id is RESTRICT, not SET NULL. Measured: SET NULL collides with the
-- COALESCE unique index the moment a NULL-keyed row already exists for the same
-- (account_id, day) -- "UNIQUE constraint failed: usage_daily_scope_uniq". RESTRICT
-- also matches wallets/ledger/topups and enforces "no hard deletes" for a key that
-- has billing history. See section 4.10.
CREATE TABLE usage_daily (
  account_id        TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
  api_key_id        TEXT REFERENCES api_keys(id) ON DELETE RESTRICT,
  day               TEXT NOT NULL CHECK (day GLOB '????-??-??'),
  input_tokens      INTEGER NOT NULL DEFAULT 0,
  cache_read_tokens INTEGER NOT NULL DEFAULT 0,
  output_tokens     INTEGER NOT NULL DEFAULT 0,
  cost_idr          INTEGER NOT NULL DEFAULT 0
) STRICT;

CREATE UNIQUE INDEX usage_daily_scope_uniq
  ON usage_daily (account_id, day, COALESCE(api_key_id, ''));
CREATE INDEX usage_daily_account_day_idx ON usage_daily (account_id, day DESC);

-- Per-request rows, for the admin "recent usage" feed. Never stores prompt or
-- completion text (Launch Gate 4). Retention: 90 days, swept (plan section 11.1).
CREATE TABLE usage_events (
  id                TEXT PRIMARY KEY,
  account_id        TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
  api_key_id        TEXT REFERENCES api_keys(id) ON DELETE SET NULL,
  model             TEXT NOT NULL,
  input_tokens      INTEGER NOT NULL DEFAULT 0,
  cache_read_tokens INTEGER NOT NULL DEFAULT 0,
  output_tokens     INTEGER NOT NULL DEFAULT 0,
  cost_idr          INTEGER NOT NULL DEFAULT 0,
  ref               TEXT,
  created_at        TEXT NOT NULL CHECK (created_at GLOB '????-??-??T??:??:??*+00:00')
) STRICT;

CREATE INDEX usage_events_recent_idx ON usage_events (created_at DESC);
CREATE INDEX usage_events_account_idx ON usage_events (account_id, created_at DESC);

-- ---------------------------------------------------------------------------
-- Telegram
-- ---------------------------------------------------------------------------

CREATE TABLE link_codes (
  code       TEXT PRIMARY KEY,
  account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
  expires_at TEXT NOT NULL CHECK (expires_at GLOB '????-??-??T??:??:??*+00:00'),
  used_at    TEXT CHECK (used_at IS NULL
                         OR used_at GLOB '????-??-??T??:??:??*+00:00'),
  created_at TEXT NOT NULL CHECK (created_at GLOB '????-??-??T??:??:??*+00:00')
) STRICT;

CREATE TABLE telegram_links (
  telegram_id TEXT PRIMARY KEY,
  account_id  TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
  linked_at   TEXT NOT NULL CHECK (linked_at GLOB '????-??-??T??:??:??*+00:00')
) STRICT;

CREATE INDEX telegram_links_account_idx ON telegram_links (account_id);

-- ---------------------------------------------------------------------------
-- Reviews
-- ---------------------------------------------------------------------------

CREATE TABLE reviews (
  id           TEXT PRIMARY KEY,
  account_id   TEXT REFERENCES accounts(id) ON DELETE SET NULL,
  telegram_id  TEXT,
  rating       INTEGER NOT NULL CHECK (rating BETWEEN 1 AND 5),
  body         TEXT CHECK (length(body) <= 1000),
  is_customer  INTEGER NOT NULL DEFAULT 0 CHECK (is_customer IN (0,1)),
  withdrawn_at TEXT CHECK (withdrawn_at IS NULL
                           OR withdrawn_at GLOB '????-??-??T??:??:??*+00:00'),
  created_at   TEXT NOT NULL CHECK (created_at GLOB '????-??-??T??:??:??*+00:00'),
  updated_at   TEXT NOT NULL CHECK (updated_at GLOB '????-??-??T??:??:??*+00:00'),

  CHECK (account_id IS NOT NULL OR telegram_id IS NOT NULL)
) STRICT;

CREATE UNIQUE INDEX reviews_account_uniq ON reviews (account_id)
  WHERE account_id IS NOT NULL;
CREATE UNIQUE INDEX reviews_telegram_uniq ON reviews (telegram_id)
  WHERE account_id IS NULL;

CREATE TABLE review_history (
  id          INTEGER PRIMARY KEY,
  review_id   TEXT NOT NULL REFERENCES reviews(id) ON DELETE CASCADE,
  rating      INTEGER NOT NULL,
  body        TEXT,
  replaced_at TEXT NOT NULL CHECK (replaced_at GLOB '????-??-??T??:??:??*+00:00')
) STRICT;

CREATE TABLE review_sessions (
  telegram_id TEXT PRIMARY KEY,
  step        TEXT NOT NULL,
  rating      INTEGER,
  body        TEXT,
  editing     INTEGER NOT NULL DEFAULT 0 CHECK (editing IN (0,1)),
  expires_at  TEXT NOT NULL CHECK (expires_at GLOB '????-??-??T??:??:??*+00:00')
) STRICT;

-- ---------------------------------------------------------------------------
-- Admin audit
-- ---------------------------------------------------------------------------

-- Append-only. Written in the SAME transaction as the effect it records.
CREATE TABLE admin_audit (
  id          INTEGER PRIMARY KEY,
  operator_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE RESTRICT,
  action      TEXT NOT NULL,
  target_type TEXT NOT NULL,
  target_id   TEXT NOT NULL,
  detail      TEXT,
  created_at  TEXT NOT NULL CHECK (created_at GLOB '????-??-??T??:??:??*+00:00')
) STRICT;

CREATE INDEX admin_audit_operator_idx ON admin_audit (operator_id, created_at DESC);
CREATE INDEX admin_audit_target_idx   ON admin_audit (target_type, target_id);

-- ---------------------------------------------------------------------------
-- Abuse signals (salted IP hashes only; no raw IP is ever stored)
-- ---------------------------------------------------------------------------

CREATE TABLE key_ip_daily (
  api_key_id    TEXT NOT NULL REFERENCES api_keys(id) ON DELETE CASCADE,
  day           TEXT NOT NULL CHECK (day GLOB '????-??-??'),
  distinct_ips  INTEGER NOT NULL DEFAULT 0,
  request_count INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (api_key_id, day)
) STRICT;

CREATE TABLE key_ip_seen (
  api_key_id TEXT NOT NULL REFERENCES api_keys(id) ON DELETE CASCADE,
  day        TEXT NOT NULL CHECK (day GLOB '????-??-??'),
  ip_hash    TEXT NOT NULL,
  PRIMARY KEY (api_key_id, day, ip_hash)
) STRICT;
