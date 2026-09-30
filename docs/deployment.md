# Deployment & Migrations

How code reaches production, and how the database schema changes without breaking
a running system.

> **Stack:** Cloudflare Pages (frontend) + **edge relay VPS** + Rust on Northflank
> (API, with SQLite embedded). Relay: [`edge-relay.md`](edge-relay.md).
> See [`architecture.md`](architecture.md).

## The core problem

**Two platforms, one push, no atomic deploy.**

Cloudflare Pages and Northflank deploy independently. A push to `main` starts
both, but they finish at different times, and either can fail while the other
succeeds. So there is always a window where:

- the **new frontend** talks to the **old API**, or
- the **new API** runs against the **old schema**, or
- the **new API** exists while the **old frontend** still calls the old shape.

Every rule below exists to make that window safe.

## The three rules

### R1 — Migrations are additive and backward compatible

A migration must not break the *currently deployed* server. That means:

**Allowed in one step:**

- `ADD COLUMN` (nullable, or with a default)
- `CREATE TABLE`
- `CREATE INDEX` (`CONCURRENTLY` in production, to avoid locking)
- Adding a nullable column plus a backfill

**Never allowed in one step:**

- `DROP COLUMN`
- `RENAME COLUMN` or `RENAME TABLE`
- `ALTER COLUMN ... SET NOT NULL` on an existing populated column
- Changing a column's type
- `DROP TABLE`

Those are **two-step** operations — see expand/contract below.

### R2 — The server deploys before the frontend

The API must tolerate the **previous** frontend for at least one release.

- Never remove an endpoint the deployed frontend still calls.
- Never make an optional request field required without a deprecation window.
- Never change a response field's meaning in place — add a new field.

### R3 — The frontend never assumes an endpoint exists

During the gap the API may be older than the frontend expects. Handle `404` and
missing fields gracefully — degrade, do not crash the page.

## Expand / contract — the only safe way to make breaking changes

Every destructive change becomes three deploys.

**Example: renaming `balance_idr` to `balance_minor`.**

| Deploy | Action | Why safe |
| --- | --- | --- |
| 1 — **expand** | `ADD COLUMN balance_minor BIGINT`; write to **both**; read from the old | Old server and new server both work |
| 2 — **backfill** | `UPDATE ... SET balance_minor = balance_idr` where null; verify counts match | Data is copied while both are live |
| 3 — **switch** | Code reads/writes only the new column | Old column is now unused |
| 4 — **contract** | `DROP COLUMN balance_idr` | Safe only after nothing reads it |

**A `DROP` is a separate deploy from the code that stopped using the column.**
Collapsing steps 3 and 4 is the classic way to break a deploy window.

**For money columns specifically:** verify the backfill before switching. A
partially-backfilled balance is a wrong balance.

## Pipeline

### Trigger

Push to `main`. Feature work happens on branches; `main` is always deployable.

### Recommended gate order

```
push to main
   |
   v
[1] build + test (Rust)        -- must pass before anything deploys
   |
   v
[2] run migrations             -- against the production DB, forward-only
   |
   v
[3] deploy Rust server         -- Northflank
   |
   v
[4] health check the API       -- /health returns OK
   |
   v
[5] deploy frontend            -- Cloudflare Pages
[5b] reload relay config       -- only if nginx.conf changed (rare)
   |
   v
[6] smoke test                 -- login + balance endpoint reachable
```

**Order matters: migrations before the server, server before the frontend.** The
new server may need the new column; the new frontend may need the new endpoint.

### If a step fails

- **Migrations fail** → nothing deployed; fix and retry. Safe.
- **Server fails after migrating** → the old server is still running against the
  new schema. This is why migrations must be additive (R1). Roll back the server
  deploy; the schema can stay.
- **Frontend fails** → the new API is live with the old UI. Usually fine, because
  the API tolerates the previous frontend (R2). Roll back the frontend.

**Never roll back a migration by hand in production.** Write a new forward
migration that undoes it. Down-migrations on live data are how you lose rows.

## The server image

Step [3] ships **one image** containing **two binaries**, both built from
`server/Dockerfile`:

| Binary | In the image as | Used by |
| --- | --- | --- |
| `apikita-server` | `/usr/local/bin/apikita-server` (the entrypoint) | Step [3] — the service itself |
| `migrate` | `/usr/local/bin/apikita-migrate` | Step [2] — `sqlite migrate`, **never on server boot** |

**Build it from the REPOSITORY ROOT**, not from `server/`:

```bash
docker build -f server/Dockerfile -t apikita-server .
```

The context must include `config/`, which is at the repo root and is **baked
into the image** on purpose: it is versioned source, and a deployment mounting a
different one is a deployment running different prices. `APIKITA_CONFIG_PATH`
still overrides it.

**The pipeline, as commands:**

```bash
# [2] migrate FIRST, against the volume the API will use. The old binary is
#     still running and still writing, which is why R1 (additive migrations)
#     exists — and why this is a separate process rather than a boot step.
docker run --rm -v apikita-data:/srv/apikita/server/data \
  --entrypoint /usr/local/bin/apikita-migrate apikita-server:latest

# [3] then the new server
docker run -d --name apikita-api -p 8080:8080 \
  -v apikita-data:/srv/apikita/server/data apikita-server:latest
```

**What the image guarantees, and how each is verified in CI:**

| Property | Why | Verified by |
| --- | --- | --- |
| Runs as **uid 10001, non-root** | It holds a writable database and provider credentials in its environment | `docker run … --entrypoint id` |
| **No toolchain or package manager** | A compiler in the runtime image is attack surface with no operational use | the runtime stage copies only the two binaries |
| `/srv/apikita/server/data` exists and is **writable by the runtime user** | A fresh volume with a root-owned directory fails on first start with "unable to open database file" | the probe in the smoke step |
| A **working `HEALTHCHECK`** | An always-red probe turns a healthy deploy into a restart loop | the smoke step requires the container to *report healthy* |
| The container's shutdown is **graceful** | `tini` reaps and forwards signals to the whole process group | `ENTRYPOINT` exec form |

**One entrypoint, two verbs.** The image's `ENTRYPOINT` is `tini -- apikita-server`;
running the migration means overriding it with `--entrypoint`. There is no
`migrate` subcommand on the server binary, deliberately: `local-development.md`
is explicit that migration never happens on server boot, and a subcommand would
make that one flag away.

## Health checks

The Rust server needs a `/health` endpoint that:

- Returns 200 only when the process **and** the database are reachable.
- Does **not** require authentication.
- Does **not** hit upstream LLM providers — an upstream outage must not make the
  server look dead and trigger a restart loop.

Deploy step [4] gates on this. Without it, a bad release takes down auth for
everyone.

**The image carries a `HEALTHCHECK` that probes the same endpoint** every 30s
(`--start-period=10s`, three retries). It is the platform's liveness signal and
is deliberately the same contract as step [4] — because `/health` already
reports the database and never touches a provider, a provider outage cannot make
a healthy gateway look dead. CI asserts the container *becomes healthy* rather
than just that it started, since a probe that always fails is indistinguishable
from an application that always crashes.

## Configuration

| Setting | Platform | When read | Secret? |
| --- | --- | --- | --- |
| `PUBLIC_API_BASE_URL` | Cloudflare Pages | **Build time** | No |
| `PUBLIC_MIDTRANS_CLIENT_KEY` | Cloudflare Pages | **Build time** | No |
| `PUBLIC_MIDTRANS_ENV` | Cloudflare Pages | **Build time** | No |
| `DATABASE_URL` | Northflank | Runtime | **Yes** |
| Provider API keys | Northflank | Runtime | **Yes** |
| `MIDTRANS_SERVER_KEY` | Northflank | Runtime | **Yes** |
| `MIDTRANS_ENV` | Northflank | Runtime | No |
| Google OAuth secret | Northflank | Runtime | **Yes** |

**Pages variables are baked in at build time.** Changing one requires a rebuild,
not a restart. That is a common source of "I changed the env var and nothing
happened".

**`PUBLIC_*` is inlined into browser JavaScript.** It is not secret. Putting a real
secret there publishes it.

### `MIDTRANS_ENV` and `PUBLIC_MIDTRANS_ENV` must match

`MIDTRANS_ENV` (Northflank, read at runtime) and `PUBLIC_MIDTRANS_ENV` (Cloudflare
Pages, inlined at build time) select which Midtrans host each side talks to. They are
**two independent variables on two platforms, set at two different times**, and
**nothing enforces that they agree**. Nothing can: the Pages value is baked into the
browser bundle, so the server cannot read it at runtime.

Both sides use the same rule — only an explicit `production` selects the live host,
anything else (including unset or a typo) is sandbox — but that rule only decides what
a *given* value means. It cannot detect that the two values disagree.

A mismatch is never diagnosed as a mismatch, and the two directions are not equally
bad. The server calls Snap *before* it writes anything, and only inserts the top-up row
once that call succeeds (the Snap call and the INSERT are in `create_topup` in [`server/src/routes/account.rs`](../server/src/routes/account.rs), with the top-up row written last):

- **Server sandbox, browser production.** The server's Snap call is rejected, so no
  top-up row is ever written. Nothing is charged and nothing is left pending — the
  request just fails.
- **Server production, browser sandbox.** This is the genuinely bad direction. The
  server mints a real `snap_token`, then the browser calls `snap.pay` against the
  sandbox host, which cannot redeem it, so the payment sheet never opens. The row *was*
  written and stays `pending` forever, with no credit created — the customer is charged
  nothing, but the wallet never fills and the failure surfaces nowhere.

- The client key (`PUBLIC_MIDTRANS_CLIENT_KEY`) must belong to the **same
  environment** as `PUBLIC_MIDTRANS_ENV`. Midtrans *conventionally* prefixes keys by
  environment — `SB-Mid-client-...` for sandbox, `Mid-client-...` for production — but
  that prefix is a convention, not a documented guarantee: Midtrans does not
  contractually specify the client-key format, no reference page states the rule, and it
  is corroborated only by example — Midtrans's own sandbox demo page uses an
  `SB-Mid-client-...` key, while the bare production prefix comes from a third-party
  guide. This repo carries no client-key sample to check it against, only
  `SB-Mid-server-` placeholders. The browser's check therefore catches an **internally
  inconsistent Pages build** — `PUBLIC_MIDTRANS_ENV` disagreeing with
  `PUBLIC_MIDTRANS_CLIENT_KEY` — and reports that to the customer instead of silently
  hanging. It cannot catch a **server/Pages disagreement**: it compares two Pages values
  and cannot read the server's `MIDTRANS_ENV` (see above), so a self-consistent Pages
  pair whose key belongs to the other environment than the server passes the check.
  Keeping the server and Pages values in step is left to the operator.
- Set all three (`MIDTRANS_ENV`, `PUBLIC_MIDTRANS_ENV`, `PUBLIC_MIDTRANS_CLIENT_KEY`)
  in the **same deploy**, then rebuild Pages — changing the Pages value alone has no
  effect until a rebuild (see above).

The authoritative check is being moved server-side — a parallel change is adding the
server's own environment to the create-topup response — so this section will be updated
when that lands.

## Database backups

The wallet ledger is the business.

- **PITR, or at minimum daily snapshots**, retained off-host.
- **Test a restore before launch.** An untested backup is a belief.
- Back up **before every migration** — the cheapest rollback is a restore.
- There is no second store to back up: identity lives in the same SQLite file
  (`accounts` + `identities`), so one tested restore covers money and logins
  together — see [`backup-and-restore.md`](backup-and-restore.md).

## What can go wrong, and the response

| Failure | Effect | Response |
| --- | --- | --- |
| Server deploys, frontend does not | New API, old UI | Usually fine (R2); roll back if not |
| Migration is non-additive | Old server crashes on new schema | Never do this — R1 |
| Frontend deploys before server | Calls a 404 endpoint | R3 handles it; redeploy in order |
| Bad migration, data damaged | Possible data loss | Restore from the pre-migration backup |
| Env var changed on Pages | No effect until rebuild | Rebuild |
| `MIDTRANS_ENV` ≠ `PUBLIC_MIDTRANS_ENV` | Payment sheet never opens; no credit, silently. No top-up row at all when the server is sandbox; a row stuck `pending` when the server is production | Set both in one deploy, rebuild Pages. The browser names an inconsistent Pages build, but nothing detects the server/Pages disagreement — the operator must keep the values in step |
| Relay config changed | Not automatic — it is not part of the app deploy | SSH or a config repo; test `nginx -t` first |
| **Relay down** | **Total outage**; the backend is unreachable | It is a single point of failure — see [\`edge-relay.md\`](edge-relay.md) |
| Database volume lost | **Total loss of funds data** | Restore; this is why backups are tested |

## Open items

- [ ] CI provider — GitHub Actions assumed. Stages specified in
      [`ci-cd.md`](ci-cd.md).
- [x] Migration tool: **sqlx migrate** — [`ci-cd.md`](ci-cd.md).
- [x] Migrations run in CI, after a snapshot and before the server deploy —
      see [`ci-cd.md`](ci-cd.md).
- [ ] Rollback drill: rehearse a bad deploy and a restore before launch — procedure
      in [`backup-and-restore.md`](backup-and-restore.md).
- [ ] Staging environment, or deploy straight to production? (Currently no staging
      is specified anywhere.)