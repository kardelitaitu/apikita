#!/usr/bin/env python3
"""Empirically probe the SQLite behaviours the apikita port depends on.

Every assertion in docs/plans/sqlite-migration.md about SQLite behaviour should be
reproducible by running this. Scratch tooling, per AGENTS.md rule 1.

Run:  python .agents/sqlite-port-probe.py
"""

import sqlite3
import sys

results = []


def check(name, fn):
    try:
        outcome = fn()
        results.append((name, "PASS", outcome))
    except Exception as exc:  # noqa: BLE001
        results.append((name, "ERROR", f"{type(exc).__name__}: {exc}"))


def fresh():
    """A fresh in-memory database, foreign keys left at the DEFAULT."""
    return sqlite3.connect(":memory:")


print(f"sqlite3 library version : {sqlite3.sqlite_version}")
print(f"python sqlite3 module   : {sqlite3.version}")
print()

# ---------------------------------------------------------------------------
# 1. Foreign keys default
# ---------------------------------------------------------------------------
def t_fk_default():
    con = fresh()
    return con.execute("PRAGMA foreign_keys").fetchone()[0]


check("PRAGMA foreign_keys default (0 = OFF)", t_fk_default)


def t_fk_off_lets_orphan_through():
    con = fresh()
    con.execute("CREATE TABLE parent (id TEXT PRIMARY KEY)")
    con.execute(
        "CREATE TABLE child (id TEXT PRIMARY KEY, pid TEXT NOT NULL "
        "REFERENCES parent(id) ON DELETE CASCADE)"
    )
    con.execute("INSERT INTO child (id, pid) VALUES ('c1','ghost')")
    return "orphan INSERT accepted with FKs off"


check("FK OFF: bad reference is accepted (silent corruption)", t_fk_off_lets_orphan_through)


def t_fk_on_rejects_orphan():
    con = fresh()
    con.execute("PRAGMA foreign_keys = ON")
    con.execute("CREATE TABLE parent (id TEXT PRIMARY KEY)")
    con.execute(
        "CREATE TABLE child (id TEXT PRIMARY KEY, pid TEXT NOT NULL "
        "REFERENCES parent(id) ON DELETE CASCADE)"
    )
    try:
        con.execute("INSERT INTO child (id, pid) VALUES ('c1','ghost')")
    except sqlite3.IntegrityError:
        return "rejected once foreign_keys=ON"
    raise AssertionError("orphan accepted even with FKs on")


check("FK ON: bad reference is rejected", t_fk_on_rejects_orphan)


# ---------------------------------------------------------------------------
# 2. Composite PRIMARY KEY does not imply NOT NULL
# ---------------------------------------------------------------------------
def t_null_in_composite_pk():
    con = fresh()
    con.execute(
        "CREATE TABLE usage_daily ("
        "  account_id TEXT NOT NULL,"
        "  api_key_id TEXT,"
        "  day TEXT NOT NULL,"
        "  input_tokens INTEGER NOT NULL DEFAULT 0,"
        "  PRIMARY KEY (account_id, api_key_id, day))"
    )
    con.execute("INSERT INTO usage_daily VALUES ('a','k','2026-09-25',1)")
    con.execute("INSERT INTO usage_daily VALUES ('a',NULL,'2026-09-25',1)")
    n = con.execute("SELECT COUNT(*) FROM usage_daily").fetchone()[0]
    return f"NULL accepted in composite PK; rows={n} (Postgres would refuse the NULL)"


check("NULL is ACCEPTED in a composite PRIMARY KEY", t_null_in_composite_pk)


def t_upsert_misses_when_key_null():
    """The real failure: the upsert never matches, so every call INSERTs a new row."""
    con = fresh()
    con.execute(
        "CREATE TABLE usage_daily ("
        "  account_id TEXT NOT NULL,"
        "  api_key_id TEXT,"
        "  day TEXT NOT NULL,"
        "  input_tokens INTEGER NOT NULL DEFAULT 0,"
        "  PRIMARY KEY (account_id, api_key_id, day))"
    )
    sql = (
        "INSERT INTO usage_daily (account_id, api_key_id, day, input_tokens) "
        "VALUES (?, ?, ?, ?) "
        "ON CONFLICT (account_id, api_key_id, day) DO UPDATE "
        "SET input_tokens = input_tokens + excluded.input_tokens"
    )
    for _ in range(3):
        con.execute(sql, ("a", None, "2026-09-25", 10))
    rows = con.execute("SELECT COUNT(*), SUM(input_tokens) FROM usage_daily").fetchone()
    return f"3 identical calls -> rows={rows[0]}, total_tokens={rows[1]} (want rows=1, total=30)"


check("UPSERT with NULL key: DUPLICATES, does not accumulate", t_upsert_misses_when_key_null)


def t_upsert_works_when_key_present():
    con = fresh()
    con.execute(
        "CREATE TABLE usage_daily ("
        "  account_id TEXT NOT NULL,"
        "  api_key_id TEXT,"
        "  day TEXT NOT NULL,"
        "  input_tokens INTEGER NOT NULL DEFAULT 0,"
        "  PRIMARY KEY (account_id, api_key_id, day))"
    )
    sql = (
        "INSERT INTO usage_daily (account_id, api_key_id, day, input_tokens) "
        "VALUES (?, ?, ?, ?) "
        "ON CONFLICT (account_id, api_key_id, day) DO UPDATE "
        "SET input_tokens = input_tokens + excluded.input_tokens"
    )
    for _ in range(3):
        con.execute(sql, ("a", "k1", "2026-09-25", 10))
    return con.execute("SELECT COUNT(*), SUM(input_tokens) FROM usage_daily").fetchone()


check("UPSERT with non-NULL key: accumulates correctly", t_upsert_works_when_key_present)


def t_coalesce_unique_index_fixes_it():
    con = fresh()
    con.execute(
        "CREATE TABLE usage_daily ("
        "  account_id TEXT NOT NULL,"
        "  api_key_id TEXT,"
        "  day TEXT NOT NULL,"
        "  input_tokens INTEGER NOT NULL DEFAULT 0)"
    )
    con.execute(
        "CREATE UNIQUE INDEX usage_daily_scope_uniq "
        "ON usage_daily (account_id, day, COALESCE(api_key_id, ''))"
    )
    sql = (
        "INSERT INTO usage_daily (account_id, api_key_id, day, input_tokens) "
        "VALUES (?, ?, ?, ?) "
        "ON CONFLICT (account_id, day, COALESCE(api_key_id, '')) DO UPDATE "
        "SET input_tokens = input_tokens + excluded.input_tokens"
    )
    for _ in range(3):
        con.execute(sql, ("a", None, "2026-09-25", 10))
    return con.execute("SELECT COUNT(*), SUM(input_tokens) FROM usage_daily").fetchone()


check("COALESCE unique index + matching conflict target: FIXES it", t_coalesce_unique_index_fixes_it)


# ---------------------------------------------------------------------------
# 3. Upsert expression forms
# ---------------------------------------------------------------------------
def t_table_qualified_do_update():
    con = fresh()
    con.execute("CREATE TABLE t (k TEXT PRIMARY KEY, n INTEGER NOT NULL DEFAULT 0)")
    con.execute(
        "INSERT INTO t (k,n) VALUES ('a',1) "
        "ON CONFLICT (k) DO UPDATE SET n = t.n + excluded.n"
    )
    return con.execute("SELECT n FROM t").fetchone()[0]


check("DO UPDATE SET t.n = t.n + excluded.n  (table-qualified)", t_table_qualified_do_update)


def t_returning_on_upsert():
    con = fresh()
    con.execute("CREATE TABLE t (k TEXT PRIMARY KEY, n INTEGER NOT NULL DEFAULT 0)")
    con.execute("INSERT INTO t (k,n) VALUES ('a',1)")
    row = con.execute(
        "INSERT INTO t (k,n) VALUES ('a',5) "
        "ON CONFLICT (k) DO UPDATE SET n = n + excluded.n RETURNING n"
    ).fetchone()
    return row


check("RETURNING on an UPSERT", t_returning_on_upsert)


# ---------------------------------------------------------------------------
# 4. Data-modifying CTE  -- the ip_tracking.rs pattern
# ---------------------------------------------------------------------------
def t_data_modifying_cte():
    con = fresh()
    con.execute("CREATE TABLE seen (k TEXT PRIMARY KEY)")
    con.execute("CREATE TABLE daily (k TEXT PRIMARY KEY, n INTEGER NOT NULL DEFAULT 0)")
    sql = (
        "WITH inserted AS ("
        "  INSERT INTO seen (k) VALUES (?) ON CONFLICT DO NOTHING RETURNING 1"
        ") "
        "INSERT INTO daily (k, n) VALUES (?, (SELECT COUNT(*) FROM inserted)) "
        "ON CONFLICT (k) DO UPDATE SET n = n + (SELECT COUNT(*) FROM inserted) "
        "RETURNING n"
    )
    return con.execute(sql, ("s1", "d1")).fetchone()


check("data-modifying CTE (WITH x AS (INSERT ... RETURNING) ...)", t_data_modifying_cte)


# ---------------------------------------------------------------------------
# 5. Constructs that must be REMOVED
# ---------------------------------------------------------------------------
def t_for_update():
    con = fresh()
    con.execute("CREATE TABLE t (k TEXT PRIMARY KEY)")
    con.execute("SELECT k FROM t FOR UPDATE")
    return "accepted (unexpected)"


check("SELECT ... FOR UPDATE  (expect an error)", t_for_update)


def t_bigint_cast():
    con = fresh()
    return con.execute("SELECT 1::bigint").fetchone()


check("SELECT 1::bigint  (expect an error)", t_bigint_cast)


def t_pg_now():
    con = fresh()
    return con.execute("SELECT now()").fetchone()


check("SELECT now()  (expect an error)", t_pg_now)


def t_pg_interval():
    con = fresh()
    return con.execute("SELECT now() - interval '2 hours'").fetchone()


check("SELECT now() - interval '2 hours'  (expect an error)", t_pg_interval)


# ---------------------------------------------------------------------------
# 6. Timestamp representation -- the sqlx round-trip risk
# ---------------------------------------------------------------------------
def t_current_timestamp_format():
    con = fresh()
    v = con.execute("SELECT CURRENT_TIMESTAMP").fetchone()[0]
    return repr(v)


check("CURRENT_TIMESTAMP output format", t_current_timestamp_format)


def t_strftime_iso():
    con = fresh()
    v = con.execute("SELECT strftime('%Y-%m-%dT%H:%M:%fZ','now')").fetchone()[0]
    return repr(v)


check("strftime RFC3339 output format", t_strftime_iso)


def t_ordering_text_vs_integer():
    """Comparability: 'YYYY-MM-DD HH:MM:SS' sorts correctly as TEXT."""
    con = fresh()
    return con.execute(
        "SELECT '2026-09-25 10:00:00' < '2026-09-25 09:00:00'"
    ).fetchone()[0]


check("CURRENT_TIMESTAMP format sorts lexicographically as expected (0 = correct)", t_ordering_text_vs_integer)


# ---------------------------------------------------------------------------
# 7. Aggregate return types -- why ::bigint removal is safe
# ---------------------------------------------------------------------------
def t_sum_type_integer():
    con = fresh()
    con.execute("CREATE TABLE m (v INTEGER NOT NULL)")
    con.executemany("INSERT INTO m VALUES (?)", [(1,), (2,), (3,)])
    row = con.execute("SELECT SUM(v), typeof(SUM(v)) FROM m").fetchone()
    return row


check("SUM over INTEGER column -> typeof", t_sum_type_integer)


def t_sum_type_real():
    con = fresh()
    con.execute("CREATE TABLE m (v REAL NOT NULL)")
    con.executemany("INSERT INTO m VALUES (?)", [(1.5,), (2.0,)])
    row = con.execute("SELECT SUM(v), typeof(SUM(v)) FROM m").fetchone()
    return row


check("SUM over REAL column -> typeof (the REAL-money hazard)", t_sum_type_real)


def t_max_two_arg_is_scalar():
    """SQLite has no GREATEST; MAX(a,b) is the scalar form. Note the trap."""
    con = fresh()
    return con.execute("SELECT MAX(0, -5)").fetchone()[0]


check("MAX(0,-5) scalar (SQLite's GREATEST) — note MAX is dual-purpose", t_max_two_arg_is_scalar)


# ---------------------------------------------------------------------------
# 8. Misc
# ---------------------------------------------------------------------------
def t_true_false():
    con = fresh()
    return con.execute("SELECT TRUE, FALSE, typeof(TRUE)").fetchone()


check("TRUE / FALSE literals", t_true_false)


def t_vacuum_into():
    con = fresh()
    con.execute("CREATE TABLE t (k TEXT)")
    con.execute("INSERT INTO t VALUES ('a')")
    import tempfile, os
    p = os.path.join(tempfile.mkdtemp(), "backup.db")
    con.execute(f"VACUUM INTO '{p}'")
    return f"VACUUM INTO ok, {os.path.getsize(p)} bytes"


check("VACUUM INTO (the backup primitive)", t_vacuum_into)


def t_wal_mode():
    import tempfile, os
    p = os.path.join(tempfile.mkdtemp(), "wal.db")
    con = sqlite3.connect(p)
    m = con.execute("PRAGMA journal_mode = WAL").fetchone()[0]
    # Persistence: a second connection to the same file.
    con2 = sqlite3.connect(p)
    m2 = con2.execute("PRAGMA journal_mode").fetchone()[0]
    return f"set={m}, after reconnect={m2}"


check("journal_mode=WAL persists in the file", t_wal_mode)


def t_begin_immediate_visible():
    con = fresh()
    con.execute("BEGIN IMMEDIATE")
    con.execute("CREATE TABLE t (k TEXT)")
    con.execute("COMMIT")
    return "BEGIN IMMEDIATE accepted"


check("BEGIN IMMEDIATE accepted", t_begin_immediate_visible)


# ---------------------------------------------------------------------------
# Report
# ---------------------------------------------------------------------------
print("=" * 78)
for name, status, outcome in results:
    print(f"[{status:5}] {name}")
    print(f"          -> {outcome}")
print("=" * 78)
print(f"{len(results)} probes")
