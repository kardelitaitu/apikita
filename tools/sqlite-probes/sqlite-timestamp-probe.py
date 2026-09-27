#!/usr/bin/env python3
"""Probe 2: the timestamp representation hazard.

sqlx-sqlite 0.8.6 encodes `DateTime<Utc>` as RFC3339 with a numeric offset
(`to_rfc3339_opts(SecondsFormat::AutoSi, false)`), but SQLite's own
`CURRENT_TIMESTAMP` produces `'YYYY-MM-DD HH:MM:SS'`. Both land in the same TEXT
columns, and SQLite compares TEXT lexicographically. This measures what that costs.

Reference: ~/.cargo/registry/.../sqlx-sqlite-0.8.6/src/types/chrono.rs
  encode  DateTime<Tz>  -> to_rfc3339_opts(SecondsFormat::AutoSi, false)
  decode  TEXT         -> rfc3339, else "%F %T%.f", "%F %R", ...
  decode  INTEGER      -> Utc.timestamp_opt(v, 0)   == unix SECONDS
"""

import sqlite3

con = sqlite3.connect(":memory:")
print(f"sqlite3 {sqlite3.sqlite_version}\n")


def q(sql, args=()):
    return con.execute(sql, args).fetchone()[0]


print("=== 1. The two formats sqlx and SQLite actually produce ===")
rust_side = q("SELECT strftime('%Y-%m-%dT%H:%M:%f','2026-10-25 06:27:22') || '+00:00'")
sql_side = q("SELECT CURRENT_TIMESTAMP")
print(f"  Rust bind  (DateTime<Utc>, RFC3339+offset) : {rust_side!r}")
print(f"  SQL now()  (CURRENT_TIMESTAMP)             : {sql_side!r}")

print("\n=== 2. Mixed formats, compared lexicographically ===")
# A session expiring at 06:27 today, evaluated at 14:00 today.
# Semantically: expired 7.5 hours ago. Query asks: is it still valid?
sql = """
SELECT ? AS expires_at, ? AS now_ts,
       (? > ?) AS sql_thinks_still_valid
"""
expires = "2026-10-25T06:27:22+00:00"   # bound by Rust
now = "2026-10-25 14:00:00"             # produced by SQLite CURRENT_TIMESTAMP
row = con.execute(sql, (expires, now, expires, now)).fetchone()
print(f"  expires_at = {row[0]}")
print(f"  now        = {row[1]}")
print(f"  expires_at > now  ->  {row[2]}   (want 0; 1 means the session survives to midnight)")
print(f"  elapsed actually   ->  7h33m EXPIRED")

print("\n=== 3. Same instant, two formats, is NOT equal ===")
a = "2026-10-25T06:27:22+00:00"
b = "2026-10-25 06:27:22"
print(f"  {a!r} = {b!r}  ->  {q('SELECT ? = ?', (a, b))}   (want 1)")

print("\n=== 4. Uniform RFC3339: is ordering correct? (variable fraction, AutoSi) ===")
cases = [
    ("2026-10-25T06:27:22+00:00", "2026-10-25T06:27:22.500+00:00", 1, "22.000 < 22.500"),
    ("2026-10-25T06:27:23+00:00", "2026-10-25T06:27:22.999+00:00", 0, "23.000 > 22.999"),
    ("2026-10-25T06:27:22.100+00:00", "2026-10-25T06:27:22.090+00:00", 0, "22.100 > 22.090"),
]
for x, y, want, note in cases:
    got = q("SELECT ? < ?", (x, y))
    print(f"  {x} < {y} -> {got} (want {want})  [{note}]")

print("\n=== 5. Uniform RFC3339 with FIXED millis (belt and braces) ===")
fixed = [
    ("2026-10-25T06:27:22.000+00:00", "2026-10-25T06:27:22.500+00:00", 1),
    ("2026-10-25T06:27:23.000+00:00", "2026-10-25T06:27:22.999+00:00", 0),
]
for x, y, want in fixed:
    got = q("SELECT ? < ?", (x, y))
    print(f"  {x} < {y} -> {got} (want {want})")

print("\n=== 6. A CHECK constraint can enforce the format ===")
con.execute(
    "CREATE TABLE sessions ("
    "  id TEXT PRIMARY KEY,"
    "  expires_at TEXT NOT NULL"
    "    CHECK (expires_at LIKE '____-__-__T__:__:__.___+00:00'"
    "           OR expires_at LIKE '____-__-__T__:__:__+00:00'),"
    "  created_at TEXT NOT NULL"
    "    CHECK (created_at LIKE '____-__-__T__:__:__%')"
    ")"
)
ok = con.execute(
    "INSERT INTO sessions VALUES ('s1','2026-10-25T06:27:22+00:00','2026-09-25T06:27:22+00:00')"
)
print("  RFC3339 insert -> accepted")
for label, val in [("CURRENT_TIMESTAMP", "2026-09-25 06:27:22")]:
    try:
        con.execute("INSERT INTO sessions VALUES ('s2', ?, ?)", (val, val))
        print(f"  {label} insert -> ACCEPTED (guard failed)")
    except sqlite3.IntegrityError as exc:
        print(f"  {label} insert -> REFUSED by CHECK: {exc}")

print("\n=== 7. INTEGER epoch alternative ===")
print(f"  unixepoch()            -> {q('SELECT unixepoch()')}  typeof={q('SELECT typeof(unixepoch())')}")
print(f"  sqlx decodes INTEGER as unix SECONDS -> DateTime<Utc> works")
print(f"  ordering is numeric    -> {q('SELECT 1792900042 < 1792900043')}")
print("  cost: every Rust bind must be i64 (.timestamp()), and sqlite3 CLI shows numbers")

print("\n=== 8. Julian-day float (what SQLite's date functions use) ===")
print(f"  julianday('now')       -> {q(chr(83)+'ELECT julianday(1)') if False else q('SELECT julianday(\"2026-09-25\")')}")
print("  sqlx can decode Float as a Julian day -- but this is REAL, so it is money-adjacent; avoid")
