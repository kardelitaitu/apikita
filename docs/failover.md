# Upstream Failover & Circuit Breaking

How the proxy behaves when an upstream provider degrades. The whitepaper promises
"traffic seamlessly shifts to secondary fallback channels instantly" but does not
specify it — **and that promise is not currently achievable** (see
[The problem with the whitepaper's promise](#the-problem-with-the-whitepapers-promise)).

> Config: [`config/apikita.toml`](../config/apikita.toml)
> API surface: [`docs/server/api-spec.md`](server/api-spec.md)

## The problem with the whitepaper's promise

The whitepaper (section 4.3) describes automatic failover across endpoints after
>3 consecutive 5xx errors. Two realities break the simple version:

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

## Layer 2 — Key pool rotation (works today)

Within one endpoint, several keys share the load.

| Trigger | Action |
| --- | --- |
| **429** or concurrency-limit error | That **key** goes on cooldown (5s); retry on another key |
| 5xx or timeout | **Endpoint** problem — handled by the circuit, not the pool |

**Attempts are capped by `max_key_attempts` (3).** Keep it <= pool size, or it
retries keys just put on cooldown.

This is what actually provides resilience **today**, since there is only one
provider. Concurrency is the real limit, per the provider's own ceiling.

## Layer 3 — Cross-provider failover (not yet usable)

Config supports multiple providers with weighted endpoints. When a second provider
is genuinely added (its own company, its own terms), then:

| Situation | Behaviour |
| --- | --- |
| Endpoint circuit open | Its weight drops to 0; others absorb traffic |
| All endpoints open | Return **503** with `Retry-After` |
| Provider returns a model-not-found | Mark that endpoint unhealthy for that model |

**A provider must not be used until its resale terms are read.** The config enforces
this with `resale_permitted`; an unverified provider sits at `weight = 0` and is
never selected. See [`docs/business/05-risk.md`](business/05-risk.md) R1.

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