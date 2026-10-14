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

**Those two strings ARE the endpoint, and both are now checked at config load** —
along with the endpoint's `name`, which is what every message about it uses to say
*which* one is meant. Before that, the validator checked `weight` for finiteness and
the rate overrides for sign, which is everything except the parts that address the
provider:

| Field | Refused at load when | What it cost to discover at runtime instead |
| --- | --- | --- |
| `upstream_model` | empty or blank | `prepare_body` writes `"model": ""`; the provider refuses **every** request on that endpoint, and the operator hears it from a customer |
| `url` | not an absolute `http(s)` URL | the client rejects it as a relative address, so it arrives as a TRANSPORT error, the breaker trips, and the endpoint leaves rotation — indistinguishable from the provider being down |
| `name` | empty, or duplicated **within one model** | the routing log, the health report and a `no-endpoint-serves-model` error all identify an endpoint by name, so a blank or duplicated one makes the single message an operator needs ambiguous |
| `api_key_envs` | empty while `weight > 0` | the endpoint can never serve, its breaker is never exercised so it stays Closed forever, and it votes healthy in `all_endpoints_unhealthy` — masking a real outage |

**Keys are exempt at `weight = 0`; `url` and `upstream_model` are not.** The asymmetry
is deliberate. A placeholder names a key nobody has bought yet, so `.env.example`
cannot document it and refusing one would reject the config that ships. But every
weight-0 placeholder the shipped config carries *already* has a real url and a real
model, so enforcing those unconditionally costs nothing today and buys the guarantee
that **raising a weight is a one-line, safe edit** — `secondary` exists precisely to
be activated when a second provider's terms are read.

**None of these consult the network.** A validator that called the provider would make
starting the server depend on a third party, which is the same reason `/health` never
checks upstream. Whether a provider actually *serves* the configured name remains a
runtime fact, and a 404 from every endpoint is reported as the configuration fault it
is — see Layer 3 below.

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
| Connection refused | **3, like the others** — see the correction below |

> **The "connection refused trips in 1" row was WRONG, and the reason it survived is worth keeping.**
> MEASURED before this was corrected: the transport arm of the routing loop calls
> `CircuitBreaker::record_failure()` once, and `record_failure` takes no argument — it increments
> `consecutive_failures` and compares to the same `failure_threshold` a 5xx uses. No per-KIND
> weighting exists anywhere in the breaker, so a refused connection is worth **1 of 3**, not 1 of 1,
> and an unreachable host is probed twice more before it is taken out of rotation.
> `a_refused_connection_does_not_trip_the_breaker_in_one_attempt` drives a closed port and asserts
> the breaker is still Closed, so the row cannot drift back.
>
> **It was plausible, which is why nobody checked it.** "An unreachable host is unambiguous" is a
> real argument for a faster trip, and a reader who wanted that behaviour would have believed the
> table rather than the code. If that behaviour IS wanted, it is not a doc edit — it is a change to
> `record_failure` to weight transport failures, and this document updates in the same commit.

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
| Provider returns a model-not-found (404) | **Rotate to the next endpoint**; if every endpoint 404s, return **500** naming the configuration fault |

**A 404 now fails over, and the row above used to claim something the code did not do.**
It read "mark that endpoint unhealthy for that model", which describes a mechanism that
has never existed — there is no per-model health map, and the breaker is per-ENDPOINT, so
a per-model verdict has nowhere to live. What the code actually did was worse than the
document's gap: **every non-5xx 4xx returned immediately**, so a 404 from one provider
aborted the request instead of trying the next one.

That was wrong for a structural reason. `upstream_model` is rewritten **per endpoint** —
that is the entire point of the `(url + upstream_model)` pair, and the config exists so
two suppliers can spell the same product differently. So "this provider does not carry
that name" is a statement about ONE supplier and says nothing about the pool, which makes
it a failover signal; treating it as a caller error turned a working failover pool into a
hard failure on whichever provider happened to be tried first.

**The arm is now split by exactly that distinction.** A 404 records the refusal
(`endpoint=upstream_model`) and rotates; **every other 4xx still aborts immediately**,
because a malformed body is malformed at every provider and retrying it would spend a
second upstream call on a request that cannot succeed. A 404 does **not** trip the
breaker: the endpoint is up and answered, and taking it out of rotation would remove
capacity for every OTHER model it serves correctly.

When *every* routed endpoint refuses, the result is a **500** naming the configuration
fault — not a 503 (a retry cannot succeed; every endpoint is healthy) and not a 404 (the
caller's request was valid and OUR `upstream_model` is wrong). The endpoint list is
logged for the operator and withheld from the client, because it names providers.

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
| Upstream latency | `mean_latency_ms` inside each `endpoint_traffic` row on `GET /api/admin/metrics` - time-to-first-byte, and `null` for an endpoint that has served nothing |

**`/health` must NOT check upstream providers.** An upstream outage would then look
like a dead server, and a naive orchestrator would restart it in a loop. `/health`
checks the process and the database only. See [`docs/observability.md`](observability.md).

## Edge cases

| Case | Behaviour |
| --- | --- |
| Single provider, circuit open | Nothing to fail over to -> **503 + Retry-After** |
| All keys cooled down | Same — do not queue; fail fast |
| Endpoint healthy but slow | Latency, not failure. Let the timeout decide |
| Upstream returns 400 (bad request) | **Not** a provider fault — do not trip the circuit, do not fail over |
| Upstream returns 404 (model not found) | **Endpoint-scoped**: rotate to the next endpoint. All 404 -> 500 config fault |
| Upstream returns 401 | Key is bad — **park that key**, do not trip the endpoint |
| Upstream returns 403 | Rotates to the next key, but does **not** park it — see below |

**"Park" is a COOLDOWN, NOT A DISABLE, and the distinction matters.** A parked key
returns after `cooldown_seconds` and is tried again. The earlier wording here said
"disable that key", which reads as permanent and would mislead anyone reasoning about
a revoked wholesale key: it comes back.

**Which statuses park a key is a CONFIG decision — `key_pool.rate_limit_status` — and the
shipped value is `[429, 401]`.** 429 is a saturated key and 401 is a rejected one; both
must stop being the *most preferred* key, because selection is least-in-flight and a
key that fails fast holds fewer leases and is therefore picked MORE often. The circuit
breaker cannot catch either: it is per-ENDPOINT, and the endpoint is healthy.

**403 is deliberately NOT in that set, and it is a real limitation rather than an
oversight.** In this system a 403 already has a defined meaning on our OWN surface —
[`docs/error-model.md`](error-model.md) documents `model_not_allowed` as "model absent
from the key's allowlist", and that page spends a section on keeping 401 and 403 apart.
An upstream 403 is therefore most consistent with a *request*-scoped refusal — this key
may not serve this model — rather than a dead credential. Parking on every 403 would
withdraw a working key from rotation for the cooldown because of one refused model.

The cost of that choice is stated plainly rather than left implicit: **a genuinely revoked
key that answers 403 instead of 401 keeps being selected.** `rate_limit_status` is matched
on the status code alone and this layer does not parse the response body, so the two cases
cannot be told apart here. If a provider is observed to answer 403 for revocations, the
answer is to add 403 to `key_pool.rate_limit_status` — one config line — and accept that a
model-not-allowed refusal also parks the key.

**Keeping the two apart is essential.** Tripping a circuit because one customer sent
malformed JSON takes out the provider for everyone.

## Open items

- [x] Cooldown cap **900s**, multiplier **2.0** — see `config/apikita.toml`.
- [x] **'All endpoints open' fails immediately; it does not queue.** This line used to read "whether
  'all endpoints open' should queue briefly or fail immediately", which was STALE: the code has always
  failed fast, and `client.rs` cites THIS section as its reason — *"Every key is cooling, or none is
  configured. Do not queue: fail fast so the caller can retry (docs/failover.md, edge cases)"*. A doc
  citing a decision it still lists as undecided is a reader trap. The reasoning matches the edge-case
  table: a queued request holds a connection and a socket, which is the resource the cap exists to
  bound, and a caller learns more from a 503 with `Retry-After` than from a request parked behind an
  outage it cannot see.
- [ ] A second provider to make Layer 3 real (blocked on resale terms).
- [x] One global upstream timeout, **120s** — `config/apikita.toml` `[circuit_breaker]`.