# Relay contract check

Two checks for the edge relay (`../../.docker/nginx/relay.conf`), because
[`docs/edge-relay.md`](../../docs/edge-relay.md) states a failure mode that is
**silent**:

> :110 — "**A relay that buffers SSE is worse than no relay** — the UI silently stops
> updating."

A buffering relay answers **200**, looks connected, and simply stops delivering
events. Nothing errors. The dashboard freezes on the last value it received, and the
only symptom is a number that stopped moving.

**Nothing in CI looked at this file at all** before these checks, so a syntax error or
a deleted `proxy_buffering` line reached production unchallenged.

## `check.sh` — the TEXT check (no Docker)

```sh
sh tools/relay-check/check.sh [path/to/relay.conf]
```

Asserts every directive the doc calls critical is present and correct on **both**
streaming locations:

| Directive | Why it cannot be dropped |
| --- | --- |
| `proxy_buffering off` | THE line. On, nginx accumulates the stream and delivers it in bursts, or at the end. |
| `gzip off` | gzip buffers, even when `proxy_buffering` is off. |
| `chunked_transfer_encoding off` | nginx would otherwise re-chunk a stream it is passing through. |
| `proxy_read_timeout` in m/h | The server block defaults to 60s; a live stream must outlast it. A short value here means nginx **cuts the stream mid-answer**. |

**Both locations, not just `/events`.** `/v1/` streams token deltas and needs the same
treatment — a buffered token stream is an answer that appears truncated.

## `behaviour.sh` — the BEHAVIOUR check (Docker)

```sh
sh tools/relay-check/behaviour.sh [path/to/relay.conf]
```

`check.sh` proves the directives are **present**; this proves they **do what the doc
claims**. A typo in a directive name passes a text check and still buffers, so the two
are different failures.

It starts a real origin emitting an event every second, a real nginx with the
**shipped** config (only the upstream and listen port are rewritten), and times
arrival:

| Config | Arrivals | Spread | Verdict |
| --- | --- | --- | --- |
| shipped | `+0.00s, +1.00s, +2.00s` | 2.00s | STREAMED |
| `proxy_buffering on` | `+3.00s, +3.00s, +3.00s` | 0.00s | BUFFERED |

**The spread is the signal, and that is deliberate.** A test that only asked "did the
events arrive" would pass on both — they DO arrive, three seconds late and all at once,
which is exactly the frozen-then-jump the doc warns about. The client fails on a batched
arrival.

**It skips LOUDLY (exit 3) when Docker is unavailable**, never 0. A silent pass on a
machine that ran nothing is the failure mode this directory exists to catch.

## Running them by hand

```sh
sh tools/relay-check/check.sh                 # seconds, no dependencies
sh tools/relay-check/behaviour.sh             # ~30s, needs Docker
```

On Windows under Git Bash, run them through `bash -c` so `cygpath` is available; the
scripts set `MSYS_NO_PATHCONV=1` themselves and convert the config mount to a
drive-letter path, because the daemon treats an MSYS `/c/...` path as a **relative**
path and creates an empty directory at the mount point (nginx then fails with
`pread() ... Is a directory`).