-- Phase 6: identity moves off PocketBase and into this database.
--
-- The `identities` table was created empty in Phase 2 for exactly this, so most
-- of the shape is already here. This migration adds the three things Phase 6
-- needs and the original schema had no place for:
--
--   1. A `verified_at` stamp on `identities`, which is what lets the Google
--      callback ordering rule be a comparison rather than a guess.
--   2. `identity_tokens`, the single-use email tokens. The plan's Appendix A
--      defines `identities` but never says where a verification or reset link
--      lives; it cannot live on the identity row, because a resend must issue a
--      new link and invalidate the old one without touching the identity.
--   3. `auth_attempts`, the record the five sign-in/signup/reset caps in
--      `[limits]` are counted from. A cap with nothing to count is a number an
--      operator believes is working.
--
-- Every timestamp here is bound from Rust, never written in SQL, for the reason
-- the initial schema's header gives: SQLite's CURRENT_TIMESTAMP emits a format
-- that does not compare against RFC3339, which would silently extend the life of
-- a token that is supposed to expire.

-- ---------------------------------------------------------------------------
-- Identity
-- ---------------------------------------------------------------------------

-- When the address on this identity was PROVEN to belong to whoever holds it.
--
-- Distinct from `email_verified`, which is a bit and no more. The difference
-- matters for the linking rule: a Google sign-in may adopt an existing password
-- identity only if that identity's address was verified BEFORE the Google
-- identity was created. A bit cannot express "before", so the ordering would be
-- unavailable and the late-verification attack would work - an attacker registers
-- with the victim's address, never verifies it, waits for the victim to sign in
-- with Google, and the victim's own verification then retroactively authorises a
-- link that was decided earlier.
--
-- NULL means "not verified, or verified before this column existed". Either way
-- no ordering claim can be made, so the rule treats it as unverifiable.
ALTER TABLE identities ADD COLUMN verified_at TEXT
  CHECK (verified_at IS NULL OR verified_at GLOB '????-??-??T??:??:??*+00:00');

-- Backfill the rows whose bit is already set. Those rows can only have been
-- verified at or before their last update, so `updated_at` is the best available
-- lower bound. It is deliberately not `created_at`: claiming a verification that
-- happened later in the row's life would be a worse error in the direction that
-- authorises linking.
UPDATE identities SET verified_at = updated_at WHERE email_verified = 1;

-- The Google callback looks an identity up by subject, and the linking rule
-- looks one up by email; both already have indexes. This one supports the
-- enforcement question "may this identity authorise a link" cheaply when the
-- table is large.
CREATE INDEX identities_verified_idx ON identities (provider, email, verified_at);

-- ---------------------------------------------------------------------------
-- Email tokens
-- ---------------------------------------------------------------------------

-- A verification or password-reset link, as a hash.
--
-- Only the SHA-256 of the token is stored. The token itself is 32 bytes from the
-- OS entropy source, so it needs no salt or stretching - there is no dictionary
-- to run against 256 uniform bits - and a leak of this table must not hand over
-- working links.
--
-- Single use is enforced by `consumed_at`, and issuing a new token for the same
-- account and purpose deletes the previous ones. Both are deliberate: a link
-- travels through mail clients and chat history, where it is not private and
-- where it can be re-opened long after it was used.
CREATE TABLE identity_tokens (
  id          TEXT PRIMARY KEY,
  -- CASCADE, matching every other account-owned table: a purged account takes
  -- its outstanding links with it, so a swept customer cannot still be mailed a
  -- working link.
  account_id  TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
  purpose     TEXT NOT NULL CHECK (purpose IN ('verification','reset')),
  token_hash  TEXT NOT NULL,
  expires_at  TEXT NOT NULL CHECK (expires_at GLOB '????-??-??T??:??:??*+00:00'),
  -- Set at the moment the token is redeemed. NULL until then.
  consumed_at TEXT CHECK (consumed_at IS NULL OR consumed_at GLOB '????-??-??T??:??:??*+00:00'),
  created_at  TEXT NOT NULL CHECK (created_at GLOB '????-??-??T??:??:??*+00:00')
) STRICT;

-- Redemption looks up by hash alone and must not scan.
CREATE UNIQUE INDEX identity_tokens_hash_idx ON identity_tokens (token_hash);

-- Issuing a replacement and sweeping expired rows both work per account+purpose.
CREATE INDEX identity_tokens_account_purpose_idx ON identity_tokens (account_id, purpose);

-- ---------------------------------------------------------------------------
-- Authentication attempts
-- ---------------------------------------------------------------------------

-- One row per counted attempt, because the caps in `[limits]` are the only thing
-- between an attacker with a password list and an account, and a cap that is not
-- recorded cannot be enforced.
--
-- `ip_hash` follows the abuse tables above: a salted hash, never a raw address.
-- The salt is daily and process-wide (`ip_tracking::DailySalt`), so the column
-- answers "how many attempts from one origin" without the table becoming a log
-- of who signed in from where.
--
-- `account_id` is NULLABLE on purpose. A sign-in for an address that does not
-- exist must still be counted (otherwise the cap is a free oracle for which
-- addresses are registered), and at that moment there is no account to point at.
-- A login attempt is recorded with whatever it can name: always the IP, and the
-- account only when one was found.
--
-- `kind` is the four things that are capped. Login is recorded TWICE per failed
-- attempt by the caller - once against the IP and once against the account - so
-- that each cap is a single indexed count rather than a union of two conditions.
CREATE TABLE auth_attempts (
  id         TEXT PRIMARY KEY,
  ip_hash    TEXT NOT NULL,
  account_id TEXT REFERENCES accounts(id) ON DELETE CASCADE,
  kind       TEXT NOT NULL CHECK (kind IN ('login','signup','password_reset','verification_resend')),
  created_at TEXT NOT NULL CHECK (created_at GLOB '????-??-??T??:??:??*+00:00')
) STRICT;

-- The two count queries: by account (login/reset/resend) and by IP (login/signup).
CREATE INDEX auth_attempts_account_kind_idx ON auth_attempts (account_id, kind, created_at);
CREATE INDEX auth_attempts_ip_kind_idx ON auth_attempts (ip_hash, kind, created_at);
