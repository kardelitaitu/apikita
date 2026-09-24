# Realtime Contract

The live-update channel between the Rust API and the dashboard. Balance and usage
must update **without the user pressing refresh**, and must never show stale money
as if it were current.

> Transport choice and topology: [`docs/architecture.md`](architecture.md)
> Endpoint: [`docs/server/api-spec.md`](server/api-spec.md) §Live updates

## Why SSE and not WebSockets

| | SSE | WebSocket |
| --- | --- | --- |
| Direction | server -> client | bidirectional |
| Protocol | plain HTTP | upgrade handshake |
| Auto-reconnect | **built into the browser** | hand-rolled |
| Through Cloudflare | works | works |

**The traffic is one-directional** — the server pushes balance and usage; the
client writes over normal HTTP. WebSockets would be machinery for a direction we
do not use, and we would have to reimplement the reconnect logic the browser gives
us free.

## Connection

```
GET /events
Cookie: session=<opaque>        (same-site, first-party)
Accept: text/event-stream
```

- **Authenticated by the session cookie**, not a token in the query string. A token
  in a URL ends up in logs and history.
- Requires the API and site to be same-site. See [`docs/architecture.md`](architecture.md)
  on subdomains — this is why that decision matters.
- Response headers: `Content-Type: text/event-stream`, `Cache-Control: no-cache`.
  **Do not let an intermediary buffer it.**

## Events

Every event carries a monotonic **`id`**, which is what makes reconnect correct.

### balance

```
id: 1042
event: balance
data: {"balance_idr":37500}
```

Sent on: top-up settlement, usage settlement, any wallet mutation.

### usage

```
id: 1043
event: usage
data: {"input_tokens":1200,"cache_read_tokens":8000,"output_tokens":400,"cost_idr":812}
```

**Three token classes, never summed.** Cache-read is priced ~200x below output; a
merged total cannot be reconciled. These are **today's cumulative totals**, not
deltas — a delta stream is fragile across reconnects.

### key

```
id: 1044
event: key
data: {"key_id":"...","revoked_at":"..."}
```

Emitted when a key is created, edited, or revoked — so a second browser tab stays
consistent.

### heartbeat

```
: heartbeat
```

A **comment line**, not an event. Every 20-30 seconds.

**Heartbeat is not optional.** Intermediaries close idle connections, and a
silently dropped stream looks exactly like "the balance stopped changing". The
heartbeat is also how the client detects a half-open connection.

## Reconnect — the part that is usually wrong

`EventSource` reconnects automatically, but naive handling causes the two classic
bugs: **missed updates** and **double-counted updates**.

### The rule: events are absolute, not deltas

**Every event carries the full current value, never a change.** This makes a lost
event self-healing: the next one corrects the state regardless of what was missed.

If events were deltas (`+50,000`), a dropped event would silently leave the UI
wrong forever.

### Last-Event-ID

On reconnect the browser sends `Last-Event-ID`. The server should:

| Case | Response |
| --- | --- |
| Recent (id still buffered) | Replay events after that id |
| Too old / unknown | Send a **full `balance` and `usage` snapshot** immediately |

**A snapshot on connect is mandatory regardless.** Do not assume the client's
state is current just because it is connecting — it may be a fresh page load.

### On the client

```js
const es = new EventSource(API + "/events", { withCredentials: true })

es.addEventListener("balance", e => {
  render(JSON.parse(e.data))
  markLive()                      // clear any stale indicator
})

es.onerror = () => markStale()     // DO NOT clear the value — mark it old
es.onopen  = () => markLive()
```

**On error, mark the data stale. Do not blank the balance and do not keep showing
it as live.** A zeroed balance looks like the money is gone; a stale one presented
as current is worse, because the user acts on it.

## Fallback

When the stream is unavailable, poll `GET /api/me` every 30-60 seconds and keep the
stale indicator until the stream recovers.

**Never fake realtime with tight polling.** A 1-second poll is load for no benefit;
30-60 seconds is honest and cheap.

| State | UI |
| --- | --- |
| Connected, fresh | Normal |
| Reconnecting | Stale badge, keep last value |
| Offline | Stale badge + poll fallback |
| Reconnected | Snapshot replaces value, badge clears |

## Server-side emission

Events are emitted **after the transaction commits**, never inside it.

| Trigger | Event |
| --- | --- |
| Webhook settles a top-up | `balance` |
| Usage settles for a request | `balance` + `usage` |
| Key created/edited/revoked | `key` |

**Emitting before commit can announce a balance that then rolls back.** The customer
sees money appear and vanish — the single most alarming thing a financial UI can do.

## Connection accounting

Realistically one connection per open tab.

| Users | Tabs | Connections |
| --- | ---: | ---: |
| 100 | 1.5 | 150 |
| 200 | 1.5 | 300 |

**Each is cheap** — an idle SSE connection is a small buffer, not a thread. At this
scale it is not a capacity concern. See
[`docs/cost-and-sizing.md`](cost-and-sizing.md).

- Cap concurrent connections per account to bound a runaway client.
- Drop the connection on session revocation — the account may be compromised.

## What is NOT streamed

| Not streamed | Why |
| --- | --- |
| Individual request/prompt content | Privacy; see [`docs/data-retention.md`](data-retention.md) |
| Full transaction history | Poll `GET /api/topups`; not worth a push per row |
| Other accounts' data | Obviously — the stream is scoped to one account |

## Open items

- [x] Replay buffer **100 events** — `config/apikita.toml` `[realtime]`.
- [x] Connection cap **5 per account**.
- [x] Max stream lifetime **30 min**, then the client reconnects.
- [ ] Whether usage should stream per-request or in periodic batches at high volume.
