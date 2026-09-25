# PostgreSQL container

Postgres is the **money** store: wallets, the append-only ledger, top-ups, usage,
API keys. PocketBase holds identity and nothing else.

## Migrations run automatically

The compose file mounts the repository's migration directory read-only:

```yaml
- ./server/migrations:/docker-entrypoint-initdb.d:ro
```

The official Postgres image executes every `*.sql` in
`/docker-entrypoint-initdb.d` in filename order — **but only when the data
directory is empty**, i.e. on the first boot of a fresh volume. Files are applied
in lexicographic order, which is why they are named with a
`YYYYMMDDHHMMSS_` timestamp prefix.

Schema source of truth: [`docs/website/02-data-model.md`](../../docs/website/02-data-model.md).

## The caveat: a new migration is NOT picked up by a restart

`docker compose restart postgres` re-runs **nothing**. `initdb.d` is a
first-boot hook, not a migration runner. A migration added after the volume was
created has to be applied by hand:

```bash
docker compose exec -T postgres psql -U postgres -d apikita \
  -f /docker-entrypoint-initdb.d/<new_migration>.sql
```

(In production, migrations are a deploy stage with a gate — see
[`docs/deployment.md`](../../docs/deployment.md) and
[`docs/ci-cd.md`](../../docs/ci-cd.md). This hook is a local convenience only.)

## Resetting the volume

```bash
docker compose down -v      # -v deletes the volumes: ALL local data is gone
docker compose up -d postgres
```

The recreated volume is empty, so the full migration set replays from scratch.
That is the supported way to pick up schema changes locally — and the only
reason to run `down -v`. Never run it against a database you care about.

**This volume is local scratch data. It is not a backup and nothing here is
recoverable** — see [`docs/backup-and-restore.md`](../../docs/backup-and-restore.md)
for what is.

## Connecting

```
postgresql://postgres:dev@localhost:5432/apikita
```

`POSTGRES_PASSWORD` defaults to `dev` and comes from `${POSTGRES_PASSWORD:-dev}`
in the compose file. `DATABASE_URL` in `.env` must match it. These are local
development credentials: production values live in Northflank secrets and are
never committed.
