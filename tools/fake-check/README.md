# fake-check — the two fake services do what their READMEs say

```sh
node tools/fake-check/fake-check.js
```

Runs in CI. Exit 1 if any documented behaviour does not hold, 3 if a tool could not be started.

## Why it exists

`tools/fake-upstream/` and `tools/fake-midtrans/` are consulted by hand. Comments in `server/src/` cite
them (`upstream/client.rs:667`, `:1489`), and `fake-midtrans`'s scenarios are the way the webhook path is
driven locally — but **nothing executed them**. They were documented contracts with no check.

That matters more here than it looks. The last three gate defects found in this repository were all in
tooling CI never ran — a gate that aborted at a dash syntax error, a gate whose assertion could not pass,
and a gate whose PATH manipulation was defeated by usrmerge. "Documented but never run" is the category,
and a mock is the worst place for it: **a mock that has drifted makes every test built on it meaningless.**
If `fake-midtrans` forges signatures the server rejects, its "valid signature" scenarios pass against
nothing at all.

## What it asserts

**`fake-midtrans` — the signature, against the server's own vector.** The tool claims to compute the
Midtrans signature *"exactly the way the server does"*, so the check compares it against
`VECTOR_SIG` in `server/src/money.rs:1005`, which that file describes as computed by `sha512sum` and
`hashlib` outside the crate — independent ground truth. It also asserts field order is load-bearing,
because a digest that survives swapping two fields is not a signature.

**`fake-upstream` — every row of its failure-injection table**, against a live instance:

| row | asserted |
| --- | --- |
| happy path | streams ≥3 chunks, ends `[DONE]`, carries `usage` |
| `?fail=429` / `?fail=500` | immediate status, OpenAI-shaped JSON error body |
| `x-fake-fail: 429` | the header form works |
| **precedence** | the **query flag wins** when both are present |
| `?fail=midstream` | 3–5 chunks then a cut, **no `[DONE]`**, **no usage** |
| `?fail=slow` | delays before the first chunk |
| `?tps=N` | changes the chunk rate |

## Two mistakes made while writing it, both worth knowing

**1. It compared a copy against a constant.** The first version transcribed `computeSignature` *inline*
and compared that against the server vector — so it asserted that the copy matched, and **never opened
`send-webhook.mjs`**. MUTATION-TESTED: swapping `orderId` and `statusCode` inside the real tool left the
gate **green**. It now extracts the function from the tool's source and evaluates it, because
`send-webhook.mjs` is a script that calls `main()` on import and exports nothing — requiring it would
fire a webhook. The same technique `tools/loadtest-check/sampler-probe.js` uses, for the same reason.

**2. Its assertions were invisible on success.** Only `fail()` calls were written for the upstream rows,
so a passing run printed nothing for eight documented behaviours — a reader could not tell they had run
at all. Every row now prints an `ok` line.

**A third, about the measurement rather than the tool:** reading `?fail=midstream` with `fetch()` reports
**zero** chunks, against a README promising 3–5. `fetch` discards a chunked response whose socket then
dies. A **raw socket** shows the three chunks arriving, so the check reads that row over a socket. A
client library inverting a verdict is exactly why this row is worth gating.

## Proving it can fail

A gate that cannot fail is the defect this check exists to prevent, so it was mutation-tested:

| mutation | result |
| --- | --- |
| the `429`/`500` branch returns `200` | **caught** |
| `?fail=midstream` stops cutting | **caught** |
| the signature's field order is swapped in the real tool | **caught** |

The first attempt at this missed both anchors and proved nothing, and a mutation of `errorBody`'s
`rateLimited` flag was **not** caught — correctly, because that flag only selects the error *type string*
and the status is set elsewhere. A mutation too narrow to change observable behaviour is not evidence
about the gate, and saying so is the point.

## What it does NOT check

It does not drive `fake-midtrans` end-to-end against a running server — the signature formula is checked
directly instead, which is the part that can drift silently. The webhook path itself is covered by the
server's own tests, which use their own fixtures.
