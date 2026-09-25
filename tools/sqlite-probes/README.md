# SQLite port probes

Executable evidence for [`docs/plans/sqlite-migration.md`](../../docs/plans/sqlite-migration.md).
These are **not scratch scripts** — the migration plan cites their output, so they live
in the tracked tree rather than in the gitignored `.agents/` directory.

Run all three before writing any Rust. They take seconds and they confirm the dialect
assumptions the whole port rests on.

```bash
python tools/sqlite-probes/sqlite-port-probe.py
python tools/sqlite-probes/sqlite-timestamp-probe.py
python tools/sqlite-probes/validate-migration-schema.py
```

Requires Python 3 with the standard-library `sqlite3` module. No packages.
Written against SQLite 3.53.1; the plan requires **≥ 3.37** for `STRICT` tables.

## `sqlite-port-probe.py`

The SQLite behaviours the port depends on, measured rather than recalled.

| Probe | Result | Plan reference |
| --- | --- | --- |
| `PRAGMA foreign_keys` default | `0` — **OFF** | §4.3 trap 1 |
| FK off: orphan insert | **accepted** (silent corruption) | §4.3 trap 1 |
| `NULL` in a composite `PRIMARY KEY` | **accepted** (Postgres refuses) | §4.3 trap 3 |
| `ON CONFLICT` with a `NULL` key, ×3 | **3 rows, not 1** | §4.3 trap 3 |
| `COALESCE` unique index + matching target | **1 row, correct total** | §4.3 trap 3 |
| `DO UPDATE SET t.n = t.n + excluded.n` | works | §4.2 |
| `RETURNING` on an upsert | works | §4.2 |
| **Data-modifying CTE** (`WITH x AS (INSERT … RETURNING)`) | **syntax error** | §4.7 |
| `SELECT … FOR UPDATE` | syntax error | §4.5 |
| `1::bigint` | syntax error | §4.1 |
| `now()`, `interval '2 hours'` | syntax errors | §4.1 |
| `SUM()` over `INTEGER` vs `REAL` | `integer` vs `real` | §4.6 |
| `journal_mode = WAL` persists | yes | §4.3 |
| `VACUUM INTO` inside a transaction | **refused** | §4.3 trap 4 |

## `sqlite-timestamp-probe.py`

The timestamp-representation hazard — the finding most likely to have caused a
production incident.

sqlx-sqlite 0.8.6 encodes a bound `DateTime<Utc>` as RFC3339 with a numeric offset;
SQLite's `CURRENT_TIMESTAMP` emits `'YYYY-MM-DD HH:MM:SS'`. Both land in the same `TEXT`
columns, and SQLite compares `TEXT` lexicographically.

Measured: a session whose `expires_at` was written by Rust, evaluated against a
SQLite-produced `now()`, reports **still valid 7.5 hours after it expired** — because
`'T'` (0x54) sorts after `' '` (0x20), so on the expiry date the RFC3339 value always
outranks the space-format value. Up to ~24 hours of extra session life, silently.

Also confirms that with a *uniform* RFC3339-offset format, ordering is correct even when
the fractional part varies in length. Plan reference: §4.6.

## `validate-migration-schema.py`

The schema gate, and the only script here that inspects the shipped artefact rather
than the plan. It does two jobs.

**1. Drift check.** It applies the plan's Appendix A *and* the shipped
`server/migrations/20260925000000_initial_schema.sql` to separate in-memory databases
and compares every object each one creates, normalised for comments and whitespace.
If the plan and the schema disagree, the run fails. A plan that lies about the
database is worse than no plan, and this is the only mechanism that stops the two
drifting apart after the port lands.

**2. Invariants**, asserted against the *shipped* migration — 32 checks:

- the schema has exactly 17 tables, and every one is `STRICT`;
- no `REAL`/`FLOAT`/`NUMERIC`/`DECIMAL` column exists anywhere;
- all 30 date and time columns carry a `GLOB` format check;
- no table has a `DEFAULT CURRENT_TIMESTAMP` — a default that fires writes the wrong
  format (plan §4.6);
- `REAL` and `TEXT` are refused in `INTEGER` money and flag columns;
- the non-negative balance floor holds;
- the timestamp `GLOB` check refuses the space and `Z` forms;
- `email_verified` is forced for Google identities and the password-hash pairing holds;
- `NULL` is refused in primary-key columns (`STRICT` implies `NOT NULL` there);
- `sessions.last_seen_at` is not optional, so the *"7 days idle"* half is enforceable;
- the `usage_daily` `COALESCE` upsert accumulates instead of duplicating, and a
  `NULL`-key row coexists with a keyed row for the same `(account, day)`;
- `RESTRICT` blocks deleting a funded account and a key with billing history;
- `topups.order_id` uniqueness is enforced;
- `accounts` has no `pb_user_id`.

It also proves the schema *applies* — a syntax error in either copy fails the run.

This is what caught the `ON DELETE SET NULL` collision with the `COALESCE` unique index
(plan §4.10), which reading the DDL did not reveal.

It supersedes an earlier `validate-appendix-schema.py`, which checked only the plan's
copy. Checking the plan alone cannot detect drift, because the plan is one of the two
things that can be wrong.

## Why these are tracked

`AGENTS.md` rule 1 sends *temporary* scripts to `.agents/`, and `.gitignore` keeps that
directory out of version control. These are the opposite: the migration plan links to
them as the evidence for its dialect claims, and §9 lists them as verification steps. A
cited instrument has to exist in the repository.
