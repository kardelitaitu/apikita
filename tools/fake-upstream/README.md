# Fake Upstream Provider

A local, dependency-free mock of an OpenAI-compatible upstream. It streams
`POST /v1/chat/completions` as SSE and can be told to fail on demand, so the
proxy's key cooldown, circuit breaker and mid-stream abort paths are testable
without touching a real provider.

Node stdlib only (`http`). Node 22.

```bash
node server.mjs          # or: npm start
PORT=8787 node server.mjs
```

Listens on `127.0.0.1:8787` by default. `PORT` overrides the port, `HOST` the
bind address. `GET /healthz` returns `200 ok` for readiness. Every request is
logged to stdout as `<timestamp> <method> <url> -> <chosen behaviour>`.

## Failure injection

Query flags and equivalent headers are interchangeable; **the query flag wins**
when both are present. Values are chosen per request, so a single server
instance covers every case.

| Behaviour | Query flag | Header | Effect |
| --- | --- | --- | --- |
| Happy path | *(default)* | — | Streams one word per chunk at `tps` (default 30 tokens/sec) |
| Rate limit | `?fail=429` | `x-fake-fail: 429` | Immediate `429` with an OpenAI-shaped JSON error body |
| Server error | `?fail=500` | `x-fake-fail: 500` | Immediate `500` with an OpenAI-shaped JSON error body |
| Mid-stream cut | `?fail=midstream` | `x-fake-fail: midstream` | 3–5 content chunks, then the stream ends — **no `[DONE]`, no usage block** |
| Slow first token | `?fail=slow` | `x-fake-fail: slow` | Sleeps `delay` ms (default 5000) before the first chunk |
| Token rate | `?tps=200` | `x-fake-tps: 200` | Chunks per second on the happy path |
| First-token delay | `?delay=8000` | `x-fake-delay-ms: 8000` | Delay used by `fail=slow` |
| Cut length | `?chunks=3` | `x-fake-chunks: 3` | Content chunks sent before a mid-stream cut (clamped to 3–5) |
| **Usage opt-in** | — | `x-fake-usage: omit` | The final usage block is sent only when the body asks for it, exactly as a real upstream does |

### The usage block is conditional, because that is the real upstream's behaviour

`client.rs::prepare_body` sets `stream_options: { include_usage: true }` before sending, and explains why:

> OpenAI-compatible upstreams emit the final usage block on SSE only when this opt-in is present; otherwise
> they assume a human is watching and omit it. Settlement reads that block and settles nothing without it, so
> a stream that skipped the opt-in would deliver tokens and **bill zero — the one error an arbitrage gateway
> cannot make.**

This fake used to emit the block unconditionally, so that failure was unrepresentable by the tool: nobody
could ask *"what does the gateway do when the opt-in is honoured by nobody?"*. It now reads the request body
and omits the block when the opt-in is absent — and `x-fake-usage: omit` forces the omission so a test does
not have to construct a body without it.

The server's side of this is already defended: `proxy.rs` calls `finish_status(0)` rather than settling, and
three tests cover it. What was missing was a way to **drive** it against something upstream-shaped, which is
what this row is for.

## Fixed usage numbers
| **Per-key failure** | — | `x-fake-key-fail: sk-bad=429` | That **key** gets the status; every other key streams normally. A comma-separated map (`sk-a=429,sk-b=401`) names several |

### Per-key failure, and why it exists

The rows above make a failure a property of the **request**. The server's failover doctrine makes it a
property of the **key**: a `429` (or anything in `key_pool.rate_limit_status`, which ships as `[429, 401]`)
parks **that key** and rotates to the next, while a `5xx` trips the **endpoint**. A fake that answers every
key the same way cannot tell those apart, so it could exercise only half of the doctrine.

MEASURED: `server/src/upstream/client.rs` tests key rotation with a bespoke inline `TcpListener` — see
`a_401_from_the_upstream_rotates_to_the_next_key` — because this fake never read `Authorization` at all.
The behaviour this README names first, *"the proxy's key cooldown ... paths are testable"*, was the one the
tool could not model.

`x-fake-key-fail` maps a **bearer token** to the status that key receives:

```bash
# sk-bad is parked, sk-good streams - which is what makes rotation observable
curl -sS -N http://127.0.0.1:8787/v1/chat/completions \
  -H 'authorization: Bearer sk-bad' \
  -H 'x-fake-key-fail: sk-bad=429' \
  -d '{"model":"flash","stream":true,"messages":[]}'
```

Requests with no `Authorization`, or a token the map does not name, keep the behaviour of the rows above —
so the map is opt-in and nothing existing changed. A `401` body says `invalid_api_key` rather than
`server_error`, because the server classifies on **status** and the body should not contradict it.


SSE chunk shape, terminated by `data: [DONE]`:

```
data: {"id","object":"chat.completion.chunk","choices":[{"delta":{"content":"..."},"index":0}]}
```

## Fixed usage numbers

The final chunk always carries the same `usage` object, independent of how many
words actually streamed. Billing assertions can hard-code these:

```json
{ "prompt_tokens": 11, "completion_tokens": 24, "total_tokens": 35 }
```

**These are asserted against a live response** by `tools/fake-check/`, which reads the numbers out of this
block rather than restating them — so if this table and the server change, the gate says so instead of
letting the two drift. The server's own fixture in `upstream/client.rs` carries the same three numbers, and
nothing compares THOSE two; this gate is what makes the published contract checked at all.

**And the gate's usage checks were vacuous for three rounds, until gating the opt-in found it.** The fake's
own canned content contains the word *usage* — *"settles usage from the final chunk"* — so
`/usage/.test(body)` is true of **every** stream. Three assertions were written that way: the happy path's
"carries usage", the mid-stream "no usage", and the published-numbers check. A usage block is a JSON object
**key**, so that is what is asserted now, and the row above is what found it.

**The mid-stream cut ends the stream rather than destroying the socket.** Both look identical to a client —
no `[DONE]`, no usage, a closed connection — but `socket.destroy()` discards whatever is still buffered, so
EVERY cut used to arrive one chunk short of the count this README promises. `res.end()` drains first, so the
chunk counts above are now true.

## Examples

Happy path, fast:

```bash
curl -sS -N "http://127.0.0.1:8787/v1/chat/completions?tps=200" \
  -H 'content-type: application/json' \
  -d '{"model":"deepseek-flash","stream":true,"messages":[{"role":"user","content":"hi"}]}'
```

Mid-stream cut — curl exits non-zero and `[DONE]` never arrives:

```bash
curl -sS -N "http://127.0.0.1:8787/v1/chat/completions?fail=midstream&chunks=3"; echo "curl exit: $?"
```
