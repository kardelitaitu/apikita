# Compose contract check

Validates [`docker-compose.yml`](../../docker-compose.yml), the deployment definition.

**Why it exists.** The file is what a new operator reads first and what
`docker compose up -d` obeys, and **nothing in CI validated it** — a search for
`compose` in the workflow found only comments. The nginx README told operators to run
`docker compose config` by hand, which is the manual-checklist pattern that lets a
machine-breaking edit ship.

**Why it asserts more than syntax.** `docker compose config` proves the YAML parses.
It cannot know that a mount is read-only when it must be writable, or that two
variables the file says must stay separate were collapsed. The file documents several
invariants as **load-bearing**, and a config that parses can still be wrong in ways
that fail at 03:00 rather than at review time.

```sh
sh tools/compose-check/check.sh [path/to/docker-compose.yml]
```

Each assertion quotes the comment it enforces, so an editor argues with the reasoning
rather than with the script.

| Invariant | What breaks without it |
| --- | --- |
| Scheduler `working_dir` is `/srv/apikita/server` | `reconcile.sh` resolves a relative DSN against its CWD, so the Gate 2 money check **silently never runs against the real database** and reports exit 6. |
| `DATABASE_URL` and `RECONCILE_DATABASE_URL` both set | The file says they are *deliberately separate* so a bad DSN disarms the reconciliation job **alone**. Collapsed, one bad value takes down both money-critical jobs at once. |
| The **data** mount is not read-only | The retention job is a `DELETE`; a read-only mount fails on every host with *"attempt to write a readonly database"*, so the retention promise is breached on schedule. |
| The **entrypoint** and **reconcile** mounts are read-only | This service must never write the repo's source. |
| The relay config is mounted at `/etc/nginx/conf.d/default.conf` | nginx loads `conf.d/*.conf`; anywhere else it runs its **default** config, which buffers SSE (see [`tools/relay-check`](../relay-check/README.md)). |
| The scheduler has **no** healthcheck | It serves nothing and listens on nothing, so any probe is theatre that reports unhealthy forever. |

**Skips LOUDLY (exit 3) when Docker is unavailable**, never 0 — a silent pass on a
machine that ran nothing is the failure mode this directory exists to catch.

## Mutation-tested

Five mutations, each caught with a message naming the invariant and why it matters:
`working_dir` changed, the DSNs collapsed, the entrypoint mount made writable, the
relay config mounted elsewhere, and the **data mount made read-only**. The unmodified
file passes.

The data-mount case was **missed by the first version of this check** and found by
mutation testing the check itself — which is the argument for doing it.