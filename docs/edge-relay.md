# Edge Relay

A cheap Linux VPS sits in front of the Northflank backend to absorb connection
load. **Northflank never sees raw client traffic.**

> Topology: [`docs/architecture.md`](architecture.md) — this document is the relay
> itself. Cost model: [`docs/cost-and-sizing.md`](cost-and-sizing.md).
>
> **Read [`topology.md`](topology.md) first if you are deciding whether the relay is a
> hard boundary or an optional optimisation.** They are different designs and the
> rest of this document assumes you have picked one.

## Why a relay at all

Northflank bills and scales the API instance. If every client connection lands
there directly, the backend pays for:

- Connection churn and TLS handshakes from the open internet
- Volumetric floods and port scans
- Slow-loris style connections that tie up capacity

A **relay absorbs all of that** and forwards only what survives its filtering. The
backend then only ever talks to one known peer.

**The relay is a load and abuse boundary, not a capacity increase.** Two cores will
not out-compute the backend; it removes work rather than adding throughput.

## Topology

```
client
  |
  v
Cloudflare edge          (DNS proxy - free, absorbs volumetric DDoS)
  |
  v
VPS: nginx + Docker      (2 vCPU / 4 GB - TLS, filtering, rate limits)
  |   normal path (private tunnel or allowlisted IP)
  v
Northflank: Rust API
  ^
  |   FALLBACK path: Cloudflare may route here directly
  |   if the relay is unavailable - so it is NOT firewalled
  |   to the relay alone. See topology.md.
```

## The decision that shapes everything: where TLS terminates

| Layer | Relay sees | Backend protected from | Cost |
| --- | --- | --- | --- |
| **L4 pass-through** | IPs, sizes, timing | Raw floods only | Lowest |
| **L7 termination** | Paths, headers, **API keys**, bodies | Almost everything | TLS CPU |
| **Cloudflare in front** | — | Volumetric DDoS | Free |

**Chosen: Cloudflare -> L7 nginx -> backend.** The relay terminates TLS because
that is the point: it must read the request to rate limit, reject oversized bodies,
and enforce per-key limits before the backend is touched.

**Consequence to accept: the relay sees API keys and request bodies.** It is now a
high-value host. Harden it accordingly — see [Hardening](#hardening).

### Why not L4 only

L4 is cheaper and sees less, but it cannot rate limit by API key or reject a
malformed request before it reaches the backend. Since the entire purpose is to
shield the backend from work, L7 is what delivers it.

## What the relay does

| Responsibility | Why here and not the backend |
| --- | --- |
| TLS termination | CPU spent on the cheap box, not the billed instance |
| Rate limit by IP | Blocks brute force before it reaches auth |
| Rate limit by API key | Cheapest place to shed a runaway client |
| Request size cap | A 100 MB body never reaches the backend |
| Timeouts and connection caps | Kills slow-loris at the edge |
| **SSE passthrough (unbuffered)** | See below — easy to get wrong |
| Static asset offload | Optional; Cloudflare cache is better |

### What the relay must NOT do

- **No business logic.** No billing, no balance checks, no auth decisions beyond
  transport. Two places enforcing money is one place too many.
- **No request body modification.** It is a pipe with filters.
- **No logging of API keys or bodies.** It sees them; it must not persist them.
  See [`docs/data-retention.md`](data-retention.md).

## Server-Sent Events through the relay

**This is the failure that will happen if the config is copied from a default
nginx setup.** `proxy_buffering` is **on** by default, which breaks SSE: events
sit in a buffer instead of reaching the client, and the dashboard stops updating
while looking connected.

Required for the `/events` location:

```nginx
location /events {
    proxy_pass              http://backend;
    proxy_http_version      1.1;
    proxy_set_header        Connection '';      # keep it open
    proxy_buffering         off;                # THE critical line
    proxy_cache             off;
    proxy_read_timeout      1h;                 # do not cut a live stream
    chunked_transfer_encoding off;
    gzip                    off;                # gzip buffers
}
```

**A relay that buffers SSE is worse than no relay** — the UI silently stops
updating and nothing in the logs looks wrong. This is the first thing to check
when live updates break in production but work locally.

## Rate limiting at the edge

Two tiers. **Both are flood protection, not policy** — the backend enforces the real
limits, and the key's configured `rate_limit_rpm` is billing-adjacent policy that
stays there. See [`docs/server/api-spec.md`](server/api-spec.md).

| Tier | Key | Limit | Purpose |
| --- | --- | --- | --- |
| Connection/IP | client IP | **30 req/s sustained, 60 burst** | Stop floods and brute force |
| Request/API key | `Authorization` header or key prefix | **100 req/s** | Stop a runaway client |
| Concurrent connections | client IP | **50** | Slow-loris and connection exhaustion |

### Why these numbers

- **30 req/s per IP is far above legitimate use.** A real integration is bursty but
  measured in requests per second, not hundreds. A mobile user alternating IPs is
  unaffected because each IP gets its own bucket.
- **100 req/s per key** sits above the highest `rate_limit_rpm` we would sell
  (60/min ≈ 1 req/s) by two orders of magnitude. The edge must never be the thing
  that rejects a paying customer for exceeding a limit they configured.
- **50 concurrent per IP** is generous for a browser and tight for a scraper.

**If a customer is ever rejected by the edge rather than the backend, these numbers
are wrong.** The edge limit exists to stop floods; anything else is a bug.

### nginx shape

```nginx
# per-IP rate
limit_req_zone $binary_remote_addr zone=perip:10m rate=30r/s;
# per-key rate (Authorization header is the key)
limit_req_zone $http_authorization zone=perkey:10m rate=100r/s;
# concurrent connections per IP
limit_conn_zone $binary_remote_addr zone=connperip:10m;

server {
    limit_req  zone=perip  burst=60 nodelay;
    limit_conn connperip 50;

    location /v1/ {
        limit_req zone=perkey burst=200 nodelay;
        proxy_pass http://backend;
    }
}
```

**Values live in `nginx.conf`, not in `config/apikita.toml`** — the relay is
infrastructure, and the app should not depend on a tuning knob it cannot see.

**Tune with data, not intuition.** Log rejections with the offending key/IP and
raise a limit only when a real customer hits it.

## Docker layout on the VPS

```yaml
services:
  nginx:
    image: nginx:alpine
    ports: ["80:80", "443:443"]
    volumes:
      - ./nginx.conf:/etc/nginx/nginx.conf:ro
      - ./certs:/etc/nginx/certs:ro
    restart: unless-stopped
  certbot:                      # renew via cron or a systemd timer
    image: certbot/certbot
    volumes: ["./certs:/etc/letsencrypt", "./www:/var/www/certbot"]
    entrypoint: /bin/sh -c 'trap exit TERM; while :; do certbot renew; sleep 12h; done'
```

**Nginx alone is enough.** There is no application container here by design — the
relay is infrastructure, not a service.

## Hardening

The relay is now the most exposed host with the most privileged view.

| Control | Reason |
| --- | --- |
| **Backend accepts only the relay** | Firewall Northflank to the relay's IP; otherwise the backend is reachable directly and the relay is pointless |
| SSH keys only, no passwords | Standard |
| Firewall: 80, 443, and an admin port only | Everything else closed |
| Automatic security updates | It runs 24/7 |
| Fail2ban or equivalent | Brute-force defence at the edge |
| Certificates auto-renew | An expired cert is a total outage |
| No secrets on the relay | It proxies; it does not need database or Midtrans keys |

**This row is a tradeoff, not an absolute rule.** Firewalling the backend to the
relay makes the relay a true boundary — but then relay-down is a total outage, and
automatic failover is impossible.

If you want failover, the backend stays reachable and **must be hardened on its own
terms** (its own rate limits, body caps, and Cloudflare in front). The relay then
reduces load rather than providing security.

**Decide which you want before implementing.** See [`topology.md`](topology.md) for the
failure matrix and the reasoning behind the recommendation.

## Failure modes

| Failure | Effect | Mitigation |
| --- | --- | --- |
| Relay down | **Degrades, does not stop** — traffic fails over to Northflank directly | See [`topology.md`](topology.md); the backend must be equally hardened |
| Cert expiry | Total outage, looks like a network fault | Auto-renew with monitoring |
| Relay flooded | Backend still safe; clients get errors | Cloudflare absorbs most; edge rate limits the rest |
| SSE buffered | Silent staleness, no errors | The config above; alert on stale streams |
| Backend unreachable | 502 from the relay | Health check + alert |

## Sizing

**2 vCPU / 4 GB is generous for this.** The relay is I/O-bound: it moves bytes and
terminates TLS. Memory per idle connection is small, and SSE connections are
idle most of the time.

| Load | Verdict |
| --- | --- |
| ~200 customers, normal traffic | Comfortable |
| TLS handshakes at high rate | The real CPU cost — session resumption helps |
| Volumetric flood | Cloudflare absorbs it first |

See [`docs/cost-and-sizing.md`](cost-and-sizing.md) for how this fits the budget.

## Observability

| Metric | Why |
| --- | --- |
| nginx access/error logs | The only record of rejected traffic |
| Active connections | Capacity signal |
| Upstream response time | Distinguishes relay latency from backend |
| 4xx/5xx at the relay | 502/504 mean the backend is unwell |

**Do not log request bodies or auth headers.** Token **counts** only — same rule as
the backend. See [`docs/observability.md`](observability.md).

## Open items

- [x] **No second relay** — the backend is the fallback. See [`topology.md`](topology.md).
- [ ] Exact private path to Northflank: IP allowlist or a tunnel (WireGuard)?
- [ ] Edge rate-limit values (deliberately blunt, tune after real traffic).
- [ ] Whether Cloudflare proxying is enabled for the relay's hostname.
- [ ] Certificate renewal monitoring.