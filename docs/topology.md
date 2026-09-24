# Topology: The Triangle

Three components that can each talk to each other, with the relay as an **optional
path** rather than a hard dependency.

```
                Cloudflare (free tier)
        DNS, TLS at edge, volumetric DDoS
                     |
        +------------+------------+
        |                         |
        v                         v
  VPS Relay (primary)      Northflank (fallback origin)
  nginx + Docker           Rust API + proxy
        |                         ^
        +-------------------------+
           relay forwards here normally

  Website (Cloudflare Pages) calls the same public hostname.
```

## Design intent

| Path | When used |
| --- | --- |
| Cloudflare -> **relay** -> Northflank | Normal operation |
| Cloudflare -> **Northflank** | Relay unavailable |

**The relay is the primary, not the only, path.** If it disappears, the system keeps
serving — which is the whole point of the triangle.

## The contradiction to resolve first

You cannot have both of these:

1. **The relay is a security boundary** — it shields the backend from the internet.
2. **Automatic failover** — the backend is reachable when the relay fails.

If Northflank can be reached directly, an attacker reaches it directly and **skips
the relay**. A boundary that can be stepped around is not a boundary.

**Resolution: the relay is a cost and load boundary, not a security boundary.**

| The relay IS | The relay is NOT |
| --- | --- |
| The normal, cheapest path | The only thing protecting the backend |
| A TLS handshake and flood absorber | A substitute for backend hardening |
| A place to shed bad traffic early | Insurance against an attack |

**Both endpoints must be hardened.** Northflank carries its own rate limits, body
caps, and Cloudflare in front. The relay reduces load; it does not grant immunity.

### The alternative, if you want a hard boundary

**No automatic failover.** Northflank is firewalled to the relay only, and relay
down means outage. That is a legitimate choice — it is just a different one. State
it explicitly rather than assuming the relay gives protection it cannot.

**Recommended: automatic failover.** Uptime is worth more than a boundary that a
determined attacker circumvents anyway, and Cloudflare absorbs the volumetric
attacks that a boundary would actually help with.

## Failover mechanisms

| Mechanism | Speed | Cost | Requires |
| --- | --- | --- | --- |
| **Cloudflare origin pool / load balancer** | Seconds | Paid plan | Health checks; both origins with certs |
| **DNS failover** | 60-300s (TTL) | Provider-dependent | Health-checking DNS |
| Client-side retry | Instant | Free | Client cooperates — **API clients will not** |

**Chosen: Cloudflare origin pool** if it is affordable; otherwise DNS failover with
a short TTL. Client-side retry is not viable for API traffic — a third-party SDK will
not know about your second hostname.

### What must be true for failover to work

1. **Both origins serve valid TLS for the same public hostname.** Otherwise the
   failover serves certificate errors.
2. **Northflank handles the full request directly** — same API, same auth. It already
   does; the relay adds no application behaviour. This is why the relay must contain
   **no business logic**.
3. **Health checks distinguish "relay down" from "backend down".** If the relay is up
   but Northflank is down, failing over to Northflank helps nothing.

**Point 2 is the reason the relay is a pipe with filters.** Any logic that lives only
on the relay becomes unavailable during exactly the incident it was meant to survive.

## What each component may do to the others

| From | To | May |
| --- | --- | --- |
| Website (Pages) | public hostname | CORS-safe API calls, SSE subscription |
| Relay | Northflank | forward HTTP, health check |
| Northflank | relay | **nothing.** The backend never calls the relay |
| Midtrans | public hostname | webhook POST — **must survive relay failover** |

**The Midtrans webhook is the row that matters.** If the relay is down and the
webhook lands on Northflank directly, it must still verify and credit correctly. It
does — the relay was never in the trust path — but only because the relay holds no
secrets.

## TLS

| Hop | Terminated by |
| --- | --- |
| Client -> Cloudflare | Cloudflare's certificate |
| Cloudflare -> relay | Relay's certificate (or Cloudflare origin cert) |
| Cloudflare -> Northflank | **Northflank's own certificate — required for failover** |
| Relay -> Northflank | HTTP inside the private path, or TLS if you prefer |

**Northflank needs its own valid certificate for the public hostname**, or failover
produces browser and SDK errors. This is the most commonly missed requirement.

## Health checks

| Check | Endpoint | Question |
| --- | --- | --- |
| Relay alive | relay `/healthz` | Is nginx serving? |
| Backend alive | `/health` | Is the API + database up? |

**The backend's `/health` must not check upstream LLM providers** — an upstream
outage would then look like a dead backend and trigger a pointless failover or a
restart loop. See [`docs/observability.md`](observability.md).

**The relay's health check must not depend on the backend either**, or a backend
outage makes both look dead and failover becomes a coin flip.

## Failure matrix

| Relay | Northflank | Cloudflare | Result |
| --- | --- | --- | --- |
| up | up | up | Normal |
| **down** | up | up | **Failover to Northflank. Degraded (no relay filtering) but serving** |
| up | **down** | up | Outage. Failover does not help — same backend |
| up | up | **down** | Outage. Cloudflare is in the path |
| down | down | up | Outage |

**Row 3 is the one people get wrong.** Failing a request from the relay to Northflank
when Northflank itself is down changes nothing. Health checks must be aware of what
they are actually testing.

## Cost consequences

| Component | Role in cost |
| --- | --- |
| Cloudflare | Free tier covers this |
| Relay VPS | A few dollars; keeps the billed backend instance small |
| Northflank failover | **You pay for idle capacity** — it must be up and ready |

**Failover means paying for a backend that is mostly idle.** That is the price of the
triangle. If that is unacceptable, the honest alternative is no failover and a
single path — see [`docs/cost-and-sizing.md`](cost-and-sizing.md).

## Open items

- [ ] Cloudflare origin pool (paid) vs DNS failover (cheaper, slower).
- [x] Health check **every 30s, fail over after 2 failures**.
- [ ] Whether the relay -> Northflank hop uses TLS.
- [ ] Second relay VPS, or accept relay-down degraded mode?
- [ ] Confirm Northflank can present a certificate for the public hostname.
