# Upstream Failover & Circuit Breaking

How the proxy behaves when an upstream provider degrades. The retired whitepaper
promised "traffic seamlessly shifts to secondary fallback channels instantly" but
never specified it — **and that promise is not achievable today** (see
[What failover actually is](#what-failover-actually-is)).

> Config: [`config/apikita.toml`](../config/apikita.toml)
> API surface: [`docs/server/api-spec.md`](server/api-spec.md)

## What failover actually is

The retired whitepaper (§4.3) described automatic failover across endpoints after
">3" consecutive 5xx errors — wrong twice over: it trips **at 3**
(`failure_threshold = 3`, compared `>=`), and endpoint order is **config order**,
not a weighted or randomized choice. Two realities still break the simple version:

1. **There is currently one permitted provider.** Every endpoint in the config
   shares the same upstream company, so there is nothing to fail over *to*. Three
   API keys at one provider is not three providers.
2. **Failover is not free.** Retrying a request that already produced output wastes
   upstream tokens you have already paid for, and the customer sees a delay.

**So the honest design has two layers:** circuit breaking within a provider (which
works today), and cross-provider failover (which does nothing until a second
provider exists).

## Layer 1 — Circuit breaking per endpoint

An endpoint is a `(url + upstream_model)` pair with a key pool.

| State | Behaviour |
| --- | --- |
| **Closed** (healthy) | Normal routing |
| **Open** (unhealthy) | Not selected; requests go elsewhere |
| **Half-open** (probing) | One trial request allowed through |

### Trip conditions

| Condition | Trips |
| --- | --- |
| Consecutive 5xx | 3 (config: `failure_threshold`) |
| Consecutive timeouts | 3 |
| Connection refused | 1 — an unreachable host is unambiguous |

**A 429 does NOT trip the circuit.** A throttled key is saturated, not broken; it
goes on **key cooldown**, and the pool rotates. Conflating the two means one
throttled key takes out a whole provider.

### Reset

After `cooldown_seconds` (default 30), the endpoint enters half-open. **One**
request is allowed through:

- Succeeds -> closed, counter reset.
- Fails -> open again, cooldown **doubles**, capped at **900s**.

Exponential backoff prevents hammering a provider that is down for an hour.

## Layer 2 — Key pool rotation & large-scale routing (up to 100+ keys)

Within one endpoint, multiple keys share the load. When operating with a large pool (e.g. 10 to 100 keys from a single wholesale supplier), the router uses specific design principles to maximize throughput and prevent thundering herds:

### 1. Least-Loaded Key Routing (Not Naive Random)
Pure random selection causes load clustering where one key handles 8 concurrent streams while another is idle. The router tracks in-flight streams per key via atomic counters (`AtomicUsize`) and always selects the healthiest key with the lowest in-flight load:

```rust
// Select the candidate key currently handling the fewest active streams
let key = healthy_keys
    .iter()
    .min_by_key(|k| k.in_flight.load(Ordering::Relaxed));
```

### 2. HTTP Connection Pooling (`reqwest`)
Opening a fresh TLS connection per request adds 50–100ms of latency per stream. With 100 keys, the HTTP client keeps a persistent pool of idle connections open to the upstream origin:

```rust
reqwest::Client::builder()
    .pool_max_idle_per_host(100)
    .tcp_keepalive(Duration::from_secs(60))
    .build()
```

### 3. Granular Per-Key Cooldown on 429
If Key #42 hits a concurrency or rate limit and returns HTTP 429:
- **Only Key #42** is marked on a 5-second cooldown timestamp (`AtomicI64`).
- The remaining 99 keys remain active and take over incoming traffic.
- The user request is immediately retried on the next available key (up to `max_key_attempts`), completely transparent to the client.
- **A 429 never trips the endpoint circuit breaker.**

### 4. Concurrency & Throughput Scaling Matrix
With 100 keys, the proxy achieves massive parallel capacity even on a low-resource container (0.2 vCPU):

| Metric | 3 Keys (Baseline) | 100 Keys (Pooled) |
| --- | ---: | ---: |
| **Max Concurrent Streams** | 15 – 30 | **500 – 1,000** |
| **Sustained Request Rate** | 3 – 5 req/s | **100 – 200 req/s** |
| **Daily Token Throughput** | ~20M – 50M tokens | **~300M – 500M tokens** |
| **Monthly Token Volume** | ~1 Billion tokens | **~10 Billion tokens** |
| **0.2 vCPU Utilization** | < 5% | **~25% – 30%** |
| **Socket Buffer RAM** | < 2 MB | **~18 MB – 25 MB** |

### 5. Egress IP Consideration
All 100 keys originate from the single Northflank container egress IP.
* **Pre-requisite:** Verify that the upstream provider does not enforce an aggressive firewall/WAF on the IP layer (e.g. capping requests per second across the entire IP address regardless of API key). If an IP-level cap exists, traffic must be multiplexed across multiple egress IPs or VPS relay instances.

## Layer 3 — Cross-provider failover (not yet usable)

Config supports multiple providers with weighted endpoints. When a second provider
is genuinely added (its own company, its own terms), then:

| Situation | Behaviour |
| --- | --- |
| Endpoint circuit open | Its weight drops to 0; others absorb traffic |
| All endpoints open | Return **503** with `Retry-After` |
| Provider returns a model-not-found | Mark that endpoint unhealthy for that model |

**A provider must not be used until its resale terms are read.** There is no
`resale_permitted` flag — this line used to claim the config enforces the rule with
one, and no such field exists. The convention is `weight = 0`: an unverified
provider sits there and is never selected by the router. It is a convention, not a
constraint, because nothing rejects an unverified entry that someone raises to a
positive weight. `server/src/config.rs` pins the COUNT of routable endpoints against
the header in `config/apikita.toml`, so a placeholder raised to a positive weight
fails the test rather than drifting quietly — but the test cannot tell a verified
endpoint from an unverified one. See [`docs/business/05-risk.md`](business/05-risk.md)
R1 and `config/README.md`.

## What happens mid-stream

**This is the case that matters and is easy to get wrong.**

If an upstream fails **after** the response has started streaming, the client has
already received partial output.

| Failure | Behaviour |
| --- | --- |
| Upstream dies before first token | Retry on another endpoint. Safe — the customer saw nothing |
| Upstream dies mid-stream | **Do not retry.** Emit a terminal error event, close cleanly |
| Upstream stalls (no bytes) | Timeout -> treat as mid-stream death |

**Never silently retry a mid-stream failure.** A retry produces a second answer
appended to the first — duplicated or contradictory output, and the customer is
billed for both. Emitting an error is the honest outcome; the client can retry if it
knows.

### Mid-stream failure and billing

The customer received partial output. Billing follows the **upstream's reported
usage** for the tokens actually generated — see
[`docs/server/api-spec.md`](server/api-spec.md). If the upstream never reports usage
because it died, that request is **washed** (not billed), and it counts toward
`wastage` in [`docs/business/03-financial-model.md`](business/03-financial-model.md).

## Ordering with the wallet checks

Failover interacts with pre-flight reservation. Order:

```
1. authenticate key
2. model allowlist
3. rate limit
4. key spend/token limit
5. wallet balance
6. pre-flight worst-case reservation
7. route to an endpoint (circuit + weights)
8. stream
9. settle usage
```

**The reservation is taken before routing, and applies regardless of which endpoint
serves the request.** Upstream cost differs per provider, so the reservation uses
the **most expensive** endpoint in the pool — otherwise a failover to a dearer
provider can overdraw the balance.

**…but the most expensive endpoint is a floor, not a ceiling.** Settlement does not
bill the endpoint's rates: it always charges the **model's** rates (the
`calculate_token_cost_idr` call in the settlement path passes `model_cfg.rates.*` and
never an endpoint's), because once the answer has been streamed the customer pays the
product price, not the reseller's. So the hold must cover the model-rate charge as
well as the dearest endpoint's. Taking the max over endpoints alone let an endpoint
that overrides *downward* — a cheaper reseller, which is the ordinary reason to add a
per-endpoint rate at all — pull the hold **below** the charge and strand money. The
rule is therefore:

    hold = max( model-rate charge , dearest-endpoint charge )

`worst_case_reservation_idr` states it as a fold seeded with the model rate, so the
product price is the floor and an endpoint can only raise it. A config with no
overrides ties with the model, so the rule is behaviour-preserving for everything
that ships today.

**The cache-read rate must be a real discount, for the same reason.** The hold
prices the whole prompt at the input rate, because at reservation time no one knows
which prompt tokens the upstream will report as cache hits; settlement splits them
and charges the cache subset at `cache_read_peak`. The hold is a ceiling over
settlement only while `cache_read_peak <= input_peak`, and the validator now
enforces that per rate class — a cache-read rate above its input rate inverts the
ceiling and strands the hold.

## Health and observability

| Signal | Metric |
| --- | --- |
| Circuit state per endpoint | `endpoint_circuit_state{endpoint}` |
| Trips | `endpoint_circuit_opened_total{endpoint}` |
| Key cooldowns | `key_cooldown_total{endpoint}` |
| Upstream latency | `proxy_upstream_latency_ms{endpoint}` |

**`/health` must NOT check upstream providers.** An upstream outage would then look
like a dead server, and a naive orchestrator would restart it in a loop. `/health`
checks the process and the database only. See [`docs/observability.md`](observability.md).

## Edge cases

| Case | Behaviour |
| --- | --- |
| Single provider, circuit open | Nothing to fail over to -> **503 + Retry-After** |
| All keys cooled down | Same — do not queue; fail fast |
| Endpoint healthy but slow | Latency, not failure. Let the timeout decide |
| Upstream returns 400 (bad request) | **Not** a provider fault — do not trip the circuit |
| Upstream returns 401/403 | Key is bad — disable that key, do not trip the endpoint |

**Distinguishing caller error from provider error is essential.** Tripping a
circuit because one customer sent malformed JSON takes out the provider for
everyone.

## Open items

- [x] Cooldown cap **900s**, multiplier **2.0** — see `config/apikita.toml`.
- [ ] Whether 'all endpoints open' should queue briefly or fail immediately.
- [ ] A second provider to make Layer 3 real (blocked on resale terms).
- [x] One global upstream timeout, **120s** — `config/apikita.toml` `[circuit_breaker]`.