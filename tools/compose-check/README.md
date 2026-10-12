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
| **Every service has a `restart` policy** | A service that exits — a crash, a reboot, an OOM kill — stays **down** until an operator notices. For the nightly sweep that means the retention promise stops happening silently. Added in round 32: the word `restart` appeared **zero** times in this gate, and removing the policy from both services left it green. |
| **Every service bounds its logs** | Docker's default `json-file` driver has **no size limit**, so a container's log grows until the filesystem fills — and on this host a full disk takes down the SQLite database, not just the logging. Added in round 35: **zero** files in the repository mentioned a container log bound. The check also refuses `logging:` without `max-size`, which is the unbounded default with extra words. |

**Skips LOUDLY (exit 3) when Docker is unavailable**, never 0 — a silent pass on a
machine that ran nothing is the failure mode this directory exists to catch.

## Mutation-tested

Five mutations, each caught with a message naming the invariant and why it matters:
`working_dir` changed, the DSNs collapsed, the entrypoint mount made writable, the
relay config mounted elsewhere, and the **data mount made read-only**. The unmodified
file passes.

**The log-bound assertion (round 35) was verified four ways**: the unmodified file passes, and removing
`logging:` from nginx, removing it from the scheduler, or keeping it while dropping `max-size` each fail
with a message naming the service. The last case matters because `logging:` without a size limit is the
unbounded default with extra words, and a check that only looked for the key would pass it.

**The restart assertion was mutation-tested on the way in, and its FIRST VERSION WAS WRONG.** It scanned
forward from a container name and stopped at the first `restart:` anywhere after it, so removing **nginx's**
policy found the *scheduler's* and reported nginx as covered — the gate stayed green for the wrong
service. It is now scoped by the rendered YAML's indentation (services at 2 spaces, their keys at 4), and
verified to fire three ways: nginx alone, scheduler alone, and both. **A check that passes for the wrong
subject is the same defect as one that cannot fail.**

The data-mount case was **missed by the first version of this check** and found by
mutation testing the check itself — which is the argument for doing it.