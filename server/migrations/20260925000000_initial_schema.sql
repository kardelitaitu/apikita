-- Extension for UUID generation
CREATE EXTENSION IF NOT EXISTS pgcrypto;

-- Accounts: Join point between PocketBase identity and Postgres
CREATE TABLE accounts (
  id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
  pb_user_id  TEXT UNIQUE NOT NULL,
  status      TEXT NOT NULL DEFAULT 'active'
              CHECK (status IN ('active', 'suspended', 'closed')),
  is_operator BOOLEAN NOT NULL DEFAULT false,
  created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
  updated_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Wallets: Owned 1:1 by account, strictly constrained non-negative
CREATE TABLE wallets (
  account_id  UUID PRIMARY KEY REFERENCES accounts(id) ON DELETE RESTRICT,
  balance_idr BIGINT NOT NULL DEFAULT 0 CHECK (balance_idr >= 0),
  updated_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Ledger: Append-only money ledger. Never UPDATE or DELETE rows.
CREATE TABLE ledger (
  id            BIGSERIAL PRIMARY KEY,
  account_id    UUID NOT NULL REFERENCES accounts(id) ON DELETE RESTRICT,
  delta_idr     BIGINT NOT NULL,
  reason        TEXT NOT NULL
                CHECK (reason IN ('topup', 'usage', 'adjustment', 'refund')),
  ref           TEXT,
  balance_after BIGINT NOT NULL,
  created_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX ledger_account_created_idx ON ledger (account_id, created_at DESC);

-- Sessions: Server-side web sessions. Immediate revocation via revoked_at.
CREATE TABLE sessions (
  id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
  account_id  UUID NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
  token_hash  TEXT NOT NULL UNIQUE,
  expires_at  TIMESTAMPTZ NOT NULL,
  revoked_at  TIMESTAMPTZ,
  user_agent  TEXT,
  ip_hash     TEXT,
  created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX sessions_account_idx ON sessions (account_id) WHERE revoked_at IS NULL;
CREATE INDEX sessions_expires_idx ON sessions (expires_at);

-- API Keys: Looked up on hot path by SHA-256 hash.
CREATE TABLE api_keys (
  id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
  account_id      UUID NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
  key_hash        TEXT NOT NULL UNIQUE,
  prefix          TEXT NOT NULL,
  label           TEXT,
  models          JSONB NOT NULL DEFAULT '[]'::jsonb,
  spend_limit_idr BIGINT NOT NULL DEFAULT 0,
  token_limit     BIGINT NOT NULL DEFAULT 0,
  rate_limit_rpm  INTEGER NOT NULL DEFAULT 0,
  expires_at      TIMESTAMPTZ,
  last_used_at    TIMESTAMPTZ,
  revoked_at      TIMESTAMPTZ,
  created_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX api_keys_hash_idx ON api_keys (key_hash) WHERE revoked_at IS NULL;
CREATE INDEX api_keys_account_idx ON api_keys (account_id);

-- Topups: Midtrans payments. order_id is UNIQUE for idempotency.
CREATE TABLE topups (
  id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
  account_id  UUID NOT NULL REFERENCES accounts(id) ON DELETE RESTRICT,
  amount_idr  BIGINT NOT NULL CHECK (amount_idr > 0),
  order_id    TEXT NOT NULL UNIQUE,
  status      TEXT NOT NULL DEFAULT 'pending'
              CHECK (status IN ('pending', 'settled', 'denied', 'expired', 'refunded')),
  snap_token  TEXT,
  created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
  settled_at  TIMESTAMPTZ
);

CREATE INDEX topups_account_created_idx ON topups (account_id, created_at DESC);

-- Usage Daily: Token and cost aggregates for rolling windows and dashboards
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

-- Link Codes: 6-digit Telegram binding codes
CREATE TABLE link_codes (
  code       TEXT PRIMARY KEY,
  account_id UUID NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
  expires_at TIMESTAMPTZ NOT NULL,
  used_at    TIMESTAMPTZ,
  created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Telegram Links: 1:1 binding between Telegram ID and Account ID
CREATE TABLE telegram_links (
  telegram_id TEXT PRIMARY KEY,
  account_id  UUID NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
  linked_at   TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX telegram_links_account_idx ON telegram_links (account_id);

-- Reviews: Submitted via Telegram bot only
CREATE TABLE reviews (
  id           UUID PRIMARY KEY DEFAULT gen_random_uuid(),
  account_id   UUID REFERENCES accounts(id) ON DELETE SET NULL,
  telegram_id  TEXT,
  rating       SMALLINT NOT NULL CHECK (rating BETWEEN 1 AND 5),
  body         TEXT CHECK (length(body) <= 1000),
  is_customer  BOOLEAN NOT NULL DEFAULT false,
  withdrawn_at TIMESTAMPTZ,
  created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
  updated_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
  CHECK (account_id IS NOT NULL OR telegram_id IS NOT NULL)
);

CREATE UNIQUE INDEX reviews_account_uniq ON reviews (account_id)
  WHERE account_id IS NOT NULL;
CREATE UNIQUE INDEX reviews_telegram_uniq ON reviews (telegram_id)
  WHERE account_id IS NULL;

-- Review History: Auditing edits to reviews
CREATE TABLE review_history (
  id          BIGSERIAL PRIMARY KEY,
  review_id   UUID NOT NULL REFERENCES reviews(id) ON DELETE CASCADE,
  rating      SMALLINT NOT NULL,
  body        TEXT,
  replaced_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Admin Audit: Immutable operator log
CREATE TABLE admin_audit (
  id           BIGSERIAL PRIMARY KEY,
  operator_id  UUID NOT NULL REFERENCES accounts(id) ON DELETE RESTRICT,
  action       TEXT NOT NULL,
  target_type  TEXT NOT NULL,
  target_id    TEXT NOT NULL,
  detail       JSONB,
  created_at   TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX admin_audit_operator_idx ON admin_audit (operator_id, created_at DESC);
CREATE INDEX admin_audit_target_idx   ON admin_audit (target_type, target_id);

-- Key IP Tracking: Abuse signals without raw IP storage
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
  ip_hash    TEXT NOT NULL,
  PRIMARY KEY (api_key_id, day, ip_hash)
);

-- Review Sessions: Transient Telegram conversation state
CREATE TABLE review_sessions (
  telegram_id TEXT PRIMARY KEY,
  step        TEXT NOT NULL,
  rating      SMALLINT,
  body        TEXT,
  editing     BOOLEAN NOT NULL DEFAULT false,
  expires_at  TIMESTAMPTZ NOT NULL
);
