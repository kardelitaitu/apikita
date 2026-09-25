#!/usr/bin/env python3
"""Validate Appendix A of docs/plans/sqlite-migration.md against real SQLite.

Extracts the ```sql block from the appendix, applies it with foreign_keys ON, then
exercises every invariant the plan claims the schema enforces. Scratch tooling
(AGENTS.md rule 1).

Run:  python .agents/validate-appendix-schema.py
"""

import io
import os
import re
import sqlite3
import sys

DOC = os.path.join(os.path.dirname(__file__), "..", "..", "docs", "plans", "sqlite-migration.md")

text = io.open(DOC, encoding="utf-8").read()
appendix = text.split("## Appendix A")[1]
blocks = re.findall(r"```sql\n(.*?)```", appendix, re.S)
if not blocks:
    sys.exit("FAIL: no sql block found in Appendix A")
schema = blocks[-1]

con = sqlite3.connect(":memory:")
con.execute("PRAGMA foreign_keys = ON")

try:
    con.executescript(schema)
except sqlite3.Error as exc:
    sys.exit(f"FAIL: schema did not apply -> {exc}")

tables = [r[0] for r in con.execute(
    "SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")]
indexes = [r[0] for r in con.execute(
    "SELECT name FROM sqlite_master WHERE type='index' AND name NOT LIKE 'sqlite_%' ORDER BY name")]

# Every table must be STRICT.
not_strict = [r[0] for r in con.execute("SELECT name, sql FROM sqlite_master WHERE type='table'")
              if "STRICT" not in r[1]]

print(f"schema applied: {len(tables)} tables, {len(indexes)} named indexes")
print(f"  all STRICT: {'yes' if not not_strict else 'NO -> ' + ', '.join(not_strict)}")
print()

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


results.append((not not_strict, "every table is declared STRICT",
                "all 17" if not not_strict else f"missing on: {not_strict}"))

# --- seed -------------------------------------------------------------------
con.execute("INSERT INTO accounts (id, created_at, updated_at) VALUES (?,?,?)", ("a1", TS, TS))
con.execute("INSERT INTO wallets (account_id, balance_idr, updated_at) VALUES (?,?,?)",
            ("a1", 10000, TS))
con.execute("INSERT INTO api_keys (id, account_id, key_hash, prefix, created_at) "
            "VALUES (?,?,?,?,?)", ("k1", "a1", "h1", "apk_x", TS))

print("--- type enforcement (the STRICT payoff) ---")
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

print("\n--- STRICT implies NOT NULL on PRIMARY KEY columns ---")
expect("NULL refused in a composite PRIMARY KEY",
       "INSERT INTO key_ip_daily (api_key_id, day, distinct_ips, request_count) VALUES (?,?,?,?)",
       (None, DAY, 1, 1), False)
expect("NULL refused in a single-column TEXT PRIMARY KEY",
       "INSERT INTO key_ip_seen (api_key_id, day, ip_hash) VALUES (?,?,?)",
       ("k1", DAY, None), False)

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

print("\n--- referential integrity ---")
expect("FK: a key referencing a ghost account is refused",
       "INSERT INTO api_keys (id, account_id, key_hash, prefix, created_at) VALUES (?,?,?,?,?)",
       ("k9", "ghost", "h9", "apk_y", TS), False)
expect("RESTRICT: hard-deleting a funded account is refused",
       "DELETE FROM accounts WHERE id = ?", ("a1",), False)
expect("topups order_id uniqueness enforced",
       "INSERT INTO topups (id, account_id, amount_idr, order_id, created_at) VALUES (?,?,?,?,?)",
       ("t1", "a1", 50000, "ORD-1", TS), True)
expect("topups duplicate order_id refused",
       "INSERT INTO topups (id, account_id, amount_idr, order_id, created_at) VALUES (?,?,?,?,?)",
       ("t2", "a1", 50000, "ORD-1", TS), False)

# RESTRICT: a key with billing history cannot be hard-deleted (section 4.10).
expect("RESTRICT: deleting a key with usage_daily history is refused",
       "DELETE FROM api_keys WHERE id = ?", ("k1",), False)
# ...and once the aggregate is gone, the key delete succeeds.
con.execute("DELETE FROM usage_daily WHERE api_key_id = 'k1'")
con.execute("DELETE FROM api_keys WHERE id = 'k1'")
left = con.execute("SELECT COUNT(*) FROM api_keys WHERE id = 'k1'").fetchone()[0]
results.append((left == 0, "key delete succeeds once its usage_daily rows are gone",
                f"{left} remaining"))

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
