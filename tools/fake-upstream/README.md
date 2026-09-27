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
| Mid-stream cut | `?fail=midstream` | `x-fake-fail: midstream` | 3–5 content chunks, then the socket is destroyed — **no `[DONE]`, no usage block** |
| Slow first token | `?fail=slow` | `x-fake-fail: slow` | Sleeps `delay` ms (default 5000) before the first chunk |
| Token rate | `?tps=200` | `x-fake-tps: 200` | Chunks per second on the happy path |
| First-token delay | `?delay=8000` | `x-fake-delay-ms: 8000` | Delay used by `fail=slow` |
| Cut length | `?chunks=3` | `x-fake-chunks: 3` | Content chunks sent before a mid-stream cut (clamped to 3–5) |

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
