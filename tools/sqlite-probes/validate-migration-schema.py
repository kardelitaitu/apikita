#!/usr/bin/env python3
"""Validate the shipped SQLite migration, and prove Appendix A still matches it.

Two jobs, in this order:

  1. DRIFT CHECK, scoped to the INITIAL migration. Apply the plan's Appendix A and
     `20260925000000_initial_schema.sql` to separate in-memory databases and compare the
     objects each creates (tables, indexes, and their SQL normalised for comments and
     whitespace). If the plan and the schema disagree, the plan is a lie about the
     database, which is worse than no plan.

     The scope is deliberate: Appendix A documents the BASE schema, so asserting the
     additive migrations against it would be wrong. They are covered by job 2 instead.

  2. INVARIANTS, against the result of EVERY migration in order. The claims below -
     every table STRICT, no REAL/FLOAT/NUMERIC column anywhere, the money floor, the
     timestamp format CHECK, the usage_daily NULL-key upsert, RESTRICT - are claims about
     the shipped SCHEMA, and that schema is what all the migrations produce together.

     THIS USED TO READ ONE FILE. The script named `20260925000000_initial_schema.sql`, so
     the two additive migrations were outside every check in this repository: a new
     migration could add a non-STRICT table or a float column and pass CI silently. A
     mutation that did exactly that now fails two checks, where before it failed none.

Tracked rather than scratch: the plan cites these results as evidence, and a probe
nobody can run proves nothing (AGENTS.md rule 1 allows `.agents/` scratch, but that
directory is gitignored).

Run:  python tools/sqlite-probes/validate-migration-schema.py
"""

import io
import os
import re
import sqlite3
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.normpath(os.path.join(HERE, "..", ".."))
PLAN = os.path.join(ROOT, "docs", "plans", "sqlite-migration.md")
MIGRATION = os.path.join(ROOT, "server", "migrations", "20260925000000_initial_schema.sql")

# ---------------------------------------------------------------------------
# Load both sources
# ---------------------------------------------------------------------------

plan_text = io.open(PLAN, encoding="utf-8").read()
appendix = plan_text.split("## Appendix A")[1]
blocks = re.findall(r"```sql\n(.*?)```", appendix, re.S)
if not blocks:
    sys.exit("FAIL: no sql block found in Appendix A")
appendix_sql = blocks[-1]
migration_sql = io.open(MIGRATION, encoding="utf-8").read()


def norm(sql):
    """Normalise a CREATE statement for comparison: drop comments, collapse space."""
    sql = re.sub(r"--[^\n]*", "", sql)
    return re.sub(r"\s+", " ", sql).strip()


def objects(sql, label):
    """Apply `sql` and return {kind: {name: normalised_sql}}."""
    con = sqlite3.connect(":memory:")
    con.execute("PRAGMA foreign_keys = ON")
    try:
        con.executescript(sql)
    except sqlite3.Error as exc:
        sys.exit(f"FAIL: {label} did not apply -> {exc}")
    out = {"table": {}, "index": {}}
    for kind, name, ddl in con.execute(
        "SELECT type, name, sql FROM sqlite_master "
        "WHERE name NOT LIKE 'sqlite_%' AND sql IS NOT NULL"
    ):
        out.setdefault(kind, {})[name] = norm(ddl)
    con.close()
    return out


a_objs = objects(appendix_sql, "Appendix A")
m_objs = objects(migration_sql, "the shipped migration")

# ---------------------------------------------------------------------------
# 1. Drift check
# ---------------------------------------------------------------------------

print("=" * 78)
print("DRIFT CHECK — Appendix A vs server/migrations/20260925000000_initial_schema.sql")
print("=" * 78)

drift = []
for kind in sorted(set(a_objs) | set(m_objs)):
    a, m = a_objs.get(kind, {}), m_objs.get(kind, {})
    only_a = sorted(set(a) - set(m))
    only_m = sorted(set(m) - set(a))
    differs = sorted(n for n in set(a) & set(m) if a[n] != m[n])
    for n in only_a:
        drift.append(f"{kind} {n}: in the plan but not in the migration")
    for n in only_m:
        drift.append(f"{kind} {n}: in the migration but not in the plan")
    for n in differs:
        drift.append(f"{kind} {n}: definitions differ")
        # SHOW THE DIFFERENCE. "definitions differ" alone says something is wrong and nothing
        # about what, leaving the operator to reverse-engineer it by hand - which is what I did
        # to find a missing `rail` column, and it is not work a checker should export.
        #
        # The SQL is NORMALISED to one line before comparison, so line numbers would be
        # meaningless (every difference reports as "line 1"). Report the first differing
        # TOKEN instead, with a little context either side: that is readable whether the
        # normalised or the original form is in front of you.
        a_tokens, m_tokens = a[n].split(), m[n].split()
        for i in range(min(len(a_tokens), len(m_tokens))):
            if a_tokens[i] != m_tokens[i]:
                context = 6
                a_ctx = " ".join(a_tokens[max(0, i - context):i + context])
                m_ctx = " ".join(m_tokens[max(0, i - context):i + context])
                drift.append(f"    first difference at token {i} (column {i + 1}):")
                drift.append(f"      plan      ... {a_ctx} ...")
                drift.append(f"      migration ... {m_ctx} ...")
                break
        else:
            # One is a prefix of the other: the extra tail is the difference.
            longer, label = ((a_tokens, "plan"), (m_tokens, "migration"))[
                len(m_tokens) > len(a_tokens)]
            i = min(len(a_tokens), len(m_tokens))
            drift.append(f"    only in the {label}: {' '.join(longer[i:])}")
    print(f"  {kind:6} plan={len(a):2}  migration={len(m):2}  "
          f"{'OK' if not (only_a or only_m or differs) else 'DRIFT'}")

if drift:
    print()
    for d in drift:
        print(f"  [DRIFT] {d}")
    print()
    print("The plan and the shipped schema disagree. Fix the plan, not the checker.")
    sys.exit(1)
print("\n  no drift: the plan's Appendix A and the shipped migration are equivalent.")

# ---------------------------------------------------------------------------
# 2. Invariants, against the shipped migration
# ---------------------------------------------------------------------------

# ALL MIGRATIONS, IN ORDER - not just the first.
#
# The drift check above is deliberately scoped to the INITIAL schema: the plan's Appendix A
# documents the base schema, and asserting the additive migrations against it would be wrong.
# The INVARIANTS below are not scoped that way - "every table is STRICT" and "no REAL/FLOAT
# column anywhere" are claims about the shipped SCHEMA, and that schema is the result of
# every migration in sequence.
#
# This was a real gap: the file named ONE migration, so `20260926000000_link_redemption_attempts`
# and `20260927000000_admin_audit_recent_index` were outside every check this repository runs.
# A new migration could add a non-STRICT table or a float column and pass CI silently.
MIGRATIONS = sorted(
    os.path.join(ROOT, "server", "migrations", f)
    for f in os.listdir(os.path.join(ROOT, "server", "migrations"))
    if f.endswith(".sql")
)
if not MIGRATIONS:
    sys.exit("FAIL: no migrations found")

con = sqlite3.connect(":memory:")
con.execute("PRAGMA foreign_keys = ON")
for path in MIGRATIONS:
    try:
        con.executescript(io.open(path, encoding="utf-8").read())
    except sqlite3.Error as exc:
        sys.exit(f"FAIL: {os.path.basename(path)} did not apply: {exc}")

print()
print(f"applied {len(MIGRATIONS)} migration(s) in order:")
for path in MIGRATIONS:
    print(f"  {os.path.basename(path)}")

tables = [r[0] for r in con.execute(
    "SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name")]
indexes = [r[0] for r in con.execute(
    "SELECT name FROM sqlite_master WHERE type='index' AND name NOT LIKE 'sqlite_%' ORDER BY name")]
not_strict = [r[0] for r in con.execute("SELECT name, sql FROM sqlite_master WHERE type='table'")
              if "STRICT" not in r[1]]

print()
print(f"migration applied: {len(tables)} tables, {len(indexes)} named indexes")

TS = "2026-09-25T06:27:22.000+00:00"
DAY = "2026-09-25"
SPACE_TS = "2026-09-25 06:27:22"
results = []


def expect(name, sql, args, should_pass):
    try:
        con.execute(sql, args)
        ok, detail = should_pass, "accepted"
    except sqlite3.Error as exc:
        ok, detail = (not should_pass), f"refused ({exc})"
    results.append((ok, name, detail))


# A FLOOR, not an exact count. The count used to be pinned at 17, which was the
# number of tables in the FIRST migration - so extending this script to apply every
# migration made the assertion fail on the correct schema (18). Pinning the total
# again would mean every additive migration has to edit this line, and the point of
# the check is to catch a migration that skips the CONVENTIONS, not to ratify a number.
#
# What matters is that the schema was read at all: a floor stops the probes below
# passing over an empty or truncated schema, which is the W38/W41/W43 failure.
results.append((len(tables) >= 17, "the schema has at least 17 tables",
                f"{len(tables)}: {', '.join(tables)}"))
results.append((not not_strict, "every table is declared STRICT",
                f"all {len(tables)}" if not not_strict else f"missing on: {not_strict}"))

# No money or token column may be REAL — STRICT would reject the type name, but
# assert the *intent* too so a future non-STRICT table cannot slip through.
floatish = []
for t in tables:
    for cid, cname, ctype, *_ in con.execute(f"PRAGMA table_xinfo('{t}')"):
        if re.search(r"REAL|FLOAT|DOUB|NUMERIC|DECIMAL", ctype or "", re.I):
            floatish.append(f"{t}.{cname} {ctype}")
results.append((not floatish, "no REAL/FLOAT/NUMERIC column anywhere",
                "none" if not floatish else ", ".join(floatish)))

# Every TEXT timestamp column must carry a format CHECK. Find them by name suffix.
ts_cols = []
for t in tables:
    for cid, cname, ctype, notnull, dflt, pk, *rest in con.execute(f"PRAGMA table_xinfo('{t}')"):
        if re.search(r"(_at|expires_at)$", cname) or cname == "day":
            ts_cols.append((t, cname))
ddl = {t: con.execute("SELECT sql FROM sqlite_master WHERE name=?", (t,)).fetchone()[0] for t in tables}
unchecked = [f"{t}.{c}" for t, c in ts_cols
             if "GLOB" not in ddl[t] or f"{c} GLOB" not in ddl[t].replace("\n", " ")]
results.append((not unchecked, f"all {len(ts_cols)} date/time columns have a GLOB CHECK",
                "all guarded" if not unchecked else f"unguarded: {unchecked}"))

# No time may be written by SQL: a DEFAULT CURRENT_TIMESTAMP would emit the wrong format.
#
# WHY THIS SINGLE LINE CARRIES A WHOLE CLASS OF DEFECT. `docs/architecture.md` states that
# every timestamp is WRITTEN from Rust. The reason is format, not taste: SQLite's
# `CURRENT_TIMESTAMP` and `datetime('now')` emit `YYYY-MM-DD HH:MM:SS`, while every
# timestamp column carries a GLOB CHECK for the RFC3339 form the Rust code binds. A
# `DEFAULT CURRENT_TIMESTAMP` would therefore be a column the schema cannot populate -
# every INSERT omitting it fails the CHECK, and every INSERT supplying it makes the default
# dead. The two are mutually exclusive and this assertion is what stops a migration from
# choosing the wrong one.
#
# The check reads `ddl[t]` for EVERY table, so it is exhaustive over migrated schema rather
# than over a list of the columns anyone remembered.
#
# VERIFIED NON-VACUOUS, and the two directions are what make it evidence rather than
# decoration. On a COPY of the tree, adding `w73_probe TEXT DEFAULT CURRENT_TIMESTAMP` to a
# table (in BOTH the migration and the plan, so the Appendix-A drift check stays clean and
# this assertion is the only one that can catch it) makes exactly one check fail -
# `[FAIL] no DEFAULT CURRENT_TIMESTAMP anywhere` - and takes the script's exit code from 0
# to 1. A pristine copy exits 0. The exit code is the half that matters for CI, which runs
# this script as a step and reads nothing else: a probe that printed [FAIL] and exited 0
# would let a schema with a SQL-written default merge.
defaults = [f"{t}" for t in tables if "CURRENT_TIMESTAMP" in ddl[t].upper()]
results.append((not defaults, "no DEFAULT CURRENT_TIMESTAMP anywhere",
                "none" if not defaults else f"present on: {defaults}"))

print("\n--- type enforcement (the STRICT payoff) ---")
con.execute("INSERT INTO accounts (id, created_at, updated_at) VALUES (?,?,?)", ("a1", TS, TS))
con.execute("INSERT INTO wallets (account_id, balance_idr, updated_at) VALUES (?,?,?)", ("a1", 10000, TS))
con.execute("INSERT INTO api_keys (id, account_id, key_hash, prefix, created_at) VALUES (?,?,?,?,?)",
            ("k1", "a1", "h1", "apk_x", TS))
expect("REAL refused in an INTEGER money column",
       "INSERT INTO wallets (account_id, balance_idr, updated_at) VALUES (?,?,?)",
       ("a2", 1.5, TS), False)
expect("TEXT refused in an INTEGER money column",
       "INSERT INTO wallets (account_id, balance_idr, updated_at) VALUES (?,?,?)",
       ("a3", "lots", TS), False)
expect("TEXT refused in an INTEGER flag column",
       "INSERT INTO accounts (id, is_operator, created_at, updated_at) VALUES (?,?,?,?)",
       ("a4", "yes", TS, TS), False)
con.execute("INSERT INTO accounts (id, created_at, updated_at) VALUES (?,?,?)", ("a5", TS, TS))
expect("INTEGER money accepted",
       "INSERT INTO wallets (account_id, balance_idr, updated_at) VALUES (?,?,?)",
       ("a5", 50000, TS), True)
expect("balance floor still enforced",
       "INSERT INTO wallets (account_id, balance_idr, updated_at) VALUES (?,?,?)",
       ("a6", -1, TS), False)

print("\n--- timestamps ---")
expect("space format refused by the GLOB CHECK",
       "INSERT INTO accounts (id, created_at, updated_at) VALUES (?,?,?)",
       ("a7", SPACE_TS, SPACE_TS), False)
expect("Z form refused by the GLOB CHECK",
       "INSERT INTO accounts (id, created_at, updated_at) VALUES (?,?,?)",
       ("a8", "2026-09-25T06:27:22Z", "2026-09-25T06:27:22Z"), False)
expect("RFC3339 with offset accepted",
       "INSERT INTO accounts (id, created_at, updated_at) VALUES (?,?,?)",
       ("a9", TS, TS), True)

print("\n--- identity constraints ---")
expect("google identity must be email_verified",
       "INSERT INTO identities (id, account_id, provider, subject, email, email_verified, "
       "created_at, updated_at) VALUES (?,?,?,?,?,?,?,?)",
       ("i1", "a1", "google", "sub1", "u@example.com", 0, TS, TS), False)
expect("password identity must carry a hash",
       "INSERT INTO identities (id, account_id, provider, subject, email, email_verified, "
       "password_hash, created_at, updated_at) VALUES (?,?,?,?,?,?,?,?,?)",
       ("i2", "a1", "password", "u@example.com", "u@example.com", 0, None, TS, TS), False)
expect("password identity WITH a hash accepted",
       "INSERT INTO identities (id, account_id, provider, subject, email, email_verified, "
       "password_hash, created_at, updated_at) VALUES (?,?,?,?,?,?,?,?,?)",
       ("i3", "a1", "password", "u@example.com", "u@example.com", 0, "$argon2id$v=19$...", TS, TS), True)
expect("unknown ledger reason refused",
       "INSERT INTO ledger (account_id, delta_idr, reason, balance_after, created_at) "
       "VALUES (?,?,?,?,?)", ("a1", 100, "theft", 100, TS), False)
expect("ledger 'adjustment' accepted (the admin path)",
       "INSERT INTO ledger (account_id, delta_idr, reason, balance_after, created_at) "
       "VALUES (?,?,?,?,?)", ("a1", 100, "adjustment", 100, TS), True)

print("\n--- the reviews author/telegram invariant ---")
# WHY THIS EXISTS. `reviews.account_id` is `ON DELETE SET NULL`, and BOTH
# `website/tests/credit-expiry-claim.test.ts` and `server/src/db.rs` reason about the
# consequence. The reasoning that matters is written down as a claim about an ORPHAN:
#
#   "`account_id IS NULL` is also the partial-index predicate for Telegram-authored
#    reviews, so a row whose author left becomes indistinguishable from one written
#    through the bot."
#
# THE CLAIM IS FALSE, and this probe is where that is enforced rather than argued. The
# table carries `CHECK (account_id IS NOT NULL OR telegram_id IS NOT NULL)`. A review
# written through the WEBSITE has `telegram_id IS NULL`, so when the FK action clears
# `account_id` the row would be (NULL, NULL) - and the CHECK REFUSES the INSERT of that
# intermediate state, which refuses the ENTIRE `DELETE FROM accounts`. The author cannot
# be deleted while their review exists, so the review cannot outlive them and cannot be
# reclassified as bot-authored.
#
# MEASURED, with `PRAGMA foreign_keys=ON` (the CLI defaults to OFF, which silently made
# a first attempt at this probe meaningless - no FK action fires, the DELETE "succeeds",
# and the review still shows its account_id exactly as if it had been protected):
#     DELETE FROM accounts WHERE id='acc-nowallet';
#     Runtime error: CHECK constraint failed:
#       account_id IS NOT NULL OR telegram_id IS NOT NULL
#
# The constraint is load-bearing for the documented reasoning and was pinned by NOTHING,
# so removing it would falsify two documents with no red build. These three probes are
# that pin: the both-NULL state is impossible, a bot review is constructible, and a
# website review is constructible. If a later migration relaxes the CHECK, the first one
# fails and names why it matters.
expect("a review with NO author at all is refused (the CHECK that keeps SET NULL honest)",
       "INSERT INTO reviews (id, account_id, telegram_id, rating, is_customer, created_at, updated_at) "
       "VALUES (?,?,?,?,?,?,?)",
       ("r-none", None, None, 5, 1, TS, TS), False)
expect("a website review (account, no telegram) is accepted",
       "INSERT INTO reviews (id, account_id, telegram_id, rating, is_customer, created_at, updated_at) "
       "VALUES (?,?,?,?,?,?,?)",
       ("r-web", "a1", None, 5, 1, TS, TS), True)
expect("a bot review (telegram, no account) is accepted",
       "INSERT INTO reviews (id, account_id, telegram_id, rating, is_customer, created_at, updated_at) "
       "VALUES (?,?,?,?,?,?,?)",
       ("r-bot", None, "tg-1", 4, 0, TS, TS), True)

# And the FK ACTION itself, which is the other half: the schema must still say SET NULL,
# because a RESTRICT there would make account deletion impossible for a different reason
# and change what the documents are describing.
_reviews_ddl = con.execute(
    "SELECT sql FROM sqlite_master WHERE type='table' AND name='reviews'").fetchone()[0]
results.append(("reviews.account_id is declared ON DELETE SET NULL",
                "ON DELETE SET NULL" in _reviews_ddl.replace("\n", " "),
                "the FK action the retention documents reason about"))
results.append(("reviews_telegram_uniq is partial on account_id IS NULL",
                "reviews_telegram_uniq" in con.execute(
                    "SELECT COALESCE(group_concat(sql), '') FROM sqlite_master "
                    "WHERE type='index' AND name='reviews_telegram_uniq'").fetchone()[0],
                "the partial index that makes account_id IS NULL mean 'bot-authored'"))

print("\n--- STRICT implies NOT NULL on PRIMARY KEY columns ---")
expect("NULL refused in a composite PRIMARY KEY",
       "INSERT INTO key_ip_daily (api_key_id, day, distinct_ips, request_count) VALUES (?,?,?,?)",
       (None, DAY, 1, 1), False)
expect("NULL refused in a single-column TEXT PRIMARY KEY",
       "INSERT INTO key_ip_seen (api_key_id, day, ip_hash) VALUES (?,?,?)",
       ("k1", DAY, None), False)

print("\n--- sessions.last_seen_at (new in this schema) ---")
expect("a session without last_seen_at is refused (idle bound is not optional)",
       "INSERT INTO sessions (id, account_id, token_hash, expires_at, created_at) VALUES (?,?,?,?,?)",
       ("s1", "a1", "th1", TS, TS), False)
expect("a session WITH last_seen_at is accepted",
       "INSERT INTO sessions (id, account_id, token_hash, expires_at, last_seen_at, created_at) "
       "VALUES (?,?,?,?,?,?)", ("s2", "a1", "th2", TS, TS, TS), True)

print("\n--- the usage_daily fix under STRICT ---")
UP = ("INSERT INTO usage_daily (account_id, api_key_id, day, input_tokens) VALUES (?,?,?,?) "
      "ON CONFLICT (account_id, day, COALESCE(api_key_id, '')) DO UPDATE "
      "SET input_tokens = input_tokens + excluded.input_tokens")
for _ in range(3):
    con.execute(UP, ("a1", None, DAY, 10))
r = con.execute("SELECT COUNT(*), SUM(input_tokens) FROM usage_daily WHERE api_key_id IS NULL").fetchone()
results.append((r == (1, 30), "NULL-key upsert accumulates to 1 row / 30 tokens", f"rows={r[0]} tokens={r[1]}"))
for _ in range(3):
    con.execute(UP, ("a1", "k1", DAY, 5))
r = con.execute("SELECT COUNT(*), SUM(input_tokens) FROM usage_daily WHERE api_key_id='k1'").fetchone()
results.append((r == (1, 15), "keyed upsert accumulates to 1 row / 15 tokens", f"rows={r[0]} tokens={r[1]}"))
# A NULL-keyed row and a keyed row must coexist for the same (account, day).
r = con.execute("SELECT COUNT(*) FROM usage_daily WHERE account_id='a1' AND day=?", (DAY,)).fetchone()[0]
results.append((r == 2, "a NULL-key row and a keyed row coexist for one (account, day)", f"rows={r}"))

print("\n--- referential integrity ---")
expect("FK: a key referencing a ghost account is refused",
       "INSERT INTO api_keys (id, account_id, key_hash, prefix, created_at) VALUES (?,?,?,?,?)",
       ("k9", "ghost", "h9", "apk_y", TS), False)
expect("RESTRICT: hard-deleting a funded account is refused",
       "DELETE FROM accounts WHERE id = ?", ("a1",), False)
# `rail` is supplied on BOTH inserts, and that is load-bearing rather than tidiness.
# These two checks are a PAIR: the first proves a top-up can be written, the second proves a
# duplicate order_id cannot. Omit `rail` and the column's NOT NULL refuses BOTH rows - so the
# first check fails, and the second PASSES FOR THE WRONG REASON, on a constraint that has
# nothing to do with uniqueness. It would still pass with the UNIQUE on order_id deleted.
# That is the shape of a vacuous test: green because something else failed first.
expect("topups order_id uniqueness enforced",
       "INSERT INTO topups (id, account_id, amount_idr, order_id, rail, created_at) "
       "VALUES (?,?,?,?,?,?)",
       ("t1", "a1", 50000, "ORD-1", "midtrans", TS), True)
expect("topups duplicate order_id refused",
       "INSERT INTO topups (id, account_id, amount_idr, order_id, rail, created_at) "
       "VALUES (?,?,?,?,?,?)",
       ("t2", "a1", 50000, "ORD-1", "midtrans", TS), False)

# RESTRICT: a key with billing history cannot be hard-deleted (section 4.10).
expect("RESTRICT: deleting a key with usage_daily history is refused",
       "DELETE FROM api_keys WHERE id = ?", ("k1",), False)
con.execute("DELETE FROM usage_daily WHERE api_key_id = 'k1'")
con.execute("DELETE FROM api_keys WHERE id = 'k1'")
left = con.execute("SELECT COUNT(*) FROM api_keys WHERE id = 'k1'").fetchone()[0]
results.append((left == 0, "key delete succeeds once its usage_daily rows are gone", f"{left} remaining"))

# Was `accounts still carries pb_user_id (PocketBase owns identity until Phase 6)`,
# asserting the column EXISTS. It is now the opposite assertion, and the reason is not
# that the old one was wrong when written: PocketBase really did own identity, and this
# check really was the thing stopping Phase 2 from dropping the column early - a probe
# that fails when the schema moves ahead of the plan is doing its job.
#
# It inverted because the migration it guards moved. `20260930000000_identity_port.sql`
# drops `pb_user_id`, and the column going away IS the port: the Rust API now mints
# accounts itself and addresses them by `id`. A probe that demanded the column back
# would fail against the shipped schema and, worse, would pass against a schema that
# had regressed - it would call a successful rollback to PocketBase-owned identity a
# green run. So the check is now that the column is GONE, plus the property the drop
# exists to buy: `accounts` still has the `id`/`created_at` a signup path needs in
# order to mint a row without an identity provider.
cols = [r[1] for r in con.execute("PRAGMA table_xinfo('accounts')")]
results.append(("pb_user_id" not in cols,
                "accounts carries no pb_user_id (the Rust API owns identity)",
                ", ".join(cols)))
results.append(("id" in cols and "created_at" in cols,
                "accounts can still be minted without an identity provider",
                ", ".join(cols)))

print()
print("=" * 78)
bad = 0
for ok, name, detail in results:
    print(f"[{'PASS' if ok else 'FAIL':4}] {name}\n        -> {detail}")
    if not ok:
        bad += 1
print("=" * 78)
print(f"{len(results)} checks, {bad} failed")
sys.exit(1 if bad else 0)
