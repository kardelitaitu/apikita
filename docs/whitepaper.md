================================================================================
WHITEPAPER: CO-PLUS LLM ARBITRAGE ROUTING ENGINE
A High-Throughput, Risk-Free Arbitrage Proxy Gateway Built in Rust
================================================================================

> **Status: the ORIGINAL design — partly superseded. See
> [architecture.md](architecture.md) for what actually shipped.**
>
> This document is kept because it is the founding statement of the business, not
> because its details are current. Three classes of claim are out of date:
>
> - **The stack.** It names **Redis**, a Postgres/JSON document store, and a
>   planned crypto rail. None shipped: the datastore is **embedded SQLite** (WAL)
>   and `server/src/routes/proxy.rs` records that this build has *no* shared-state
>   dependency. The crypto rail is **not built and not promised**
>   (`decisions.md` §Money).
> - **The economics (§2.1).** Its rate card was a single price vintage **2.43x
>   below** the rate the system bills on. The verified card and the peak/off-peak
>   split are in [business/02-pricing.md](business/02-pricing.md); the figures in
>   §2.1 have been corrected in place, but the surrounding prose is the original
>   analysis.
> - **"Risk-Free" and the failover story.** Cross-provider failover is **not
>   usable** with one provider, and is described honestly in
>   [failover.md](failover.md). Nothing here is risk-free.
>
> A test pins the §2.1 figures to `config/apikita.toml`
> (`website/tests/whitepaper-pricing.test.ts`), so the rate card cannot silently
> drift again. The rest of the prose is **not** guarded — treat it as history.

1. EXECUTIVE SUMMARY
The rapid commoditization of Large Language Models (LLMs) has created a highly 
fragmented wholesale market, driven largely by hyper-competitive, ultra-low-cost 
API infrastructure providers within the Asian region (predominantly mainland China). 
While these endpoints offer base pricing significantly below official global 
baselines, Western and local regional developers face high barriers to adoption 
due to geographic latency, routing instabilities caused by cross-border network 
filters, and a lack of standardized billing interfaces. 

This project establishes a low-footprint, high-concurrency reverse-proxy gateway 
engineered entirely in Rust using the Tokio async ecosystem. Operating as a pure 
arbitrage engine, the platform transforms volatile, wholesale multi-endpoint 
infrastructure into a localized, highly reliable, unified API interface. By 
executing an automated Cost-Plus Pricing Model (+50% margin matrix) coupled with 
dynamic pre-flight wallet reservations, the network entirely eliminates 
structural financial deficits and operational liability, delivering a guaranteed 
risk-free profit margin on every token processed.

--------------------------------------------------------------------------------

2. MARKET ARBITRAGE & ECONOMIC MODEL

2.1 The Pricing Mismatch
Global LLM pricing targets a standardized retail baseline. Concurrently, regional 
Chinese infrastructure providers optimize compute clusters to offer wholesale 
pricing profiles that split charges down to the exact caching execution layer. 

By targeting models like DeepSeek-V4-Flash during optimized compute windows, 
our platform capitalizes on deep-wholesale pricing inefficiencies:

- Standard Input Tokens:
  * Wholesale Cost (Off-peak): 1,338.39 IDR / 1M
  * Wholesale Cost (Peak):     2,676.78 IDR / 1M
  * Consumer Price (+50%, at peak): 4,015 IDR / 1M

- Model Output Tokens:
  * Wholesale Cost (Off-peak): 5,353.56 IDR / 1M
  * Wholesale Cost (Peak):     10,707.12 IDR / 1M
  * Consumer Price (+50%, at peak): 16,061 IDR / 1M

- Cache Read (KV Hit) Tokens:
  * Wholesale Cost (Off-peak): 26.77 IDR / 1M
  * Wholesale Cost (Peak):     53.54 IDR / 1M
  * Consumer Price (+50%, at peak): 80 IDR / 1M

> **Corrected.** These figures previously read ~1,100 / ~4,400 / ~22 IDR per 1M —
> a single price vintage **2.43x below** the rate the system actually bills on, and
> blind to the peak/off-peak split entirely. There is no single wholesale number:
> **peak is exactly double off-peak**, and peak covers 01:00-04:00 and
> 06:00-10:00 UTC, Mon-Fri (~21% of the week). The system bills on the PEAK rate so
> a request can never lose money (`config/apikita.toml`, `billing_basis = "peak"`),
> which is the basis the margin above is stated at. Measured against that basis the
> old card understated cost by 2.43x on every token class — and a cost understated
> by 2.43x is a margin that does not exist. Source:
> [business/02-pricing.md](business/02-pricing.md).

At the launch margin of 50% on every model, the consumer rate remains highly
competitive relative to standard retail API entry points, providing a dual value
proposition: deep cost savings for the customer and guaranteed profitability for
the operator. The margin is set **per model** rather than as one global number —
pro-tier output costs several times flash, so a single rate would make one
unsellable and the other unprofitable — and every model must declare its price,
so a missing value fails at startup instead of silently applying a wrong margin
(`config/apikita.toml`).

2.2 Risk Elimination Matrix
To safely offer variable, post-request token billing under a prepaid architecture, 
the system operates a strict three-phase verification lifecycle:

Phase 1: Pre-Flight Reservation Phase
- Estimate input tokens from the request body. (**Not `tiktoken-rs` — that crate is
  not a dependency; the estimate is the documented approximation, not a tokenizer.**)
- Compute Worst-Case Cost (0% cache + Max Output tokens) at the PEAK rate, so a
  request cannot lose money on a peak/off-peak flip.
- If Wallet Balance < Worst-Case Cost -> REJECT request immediately.

Phase 2: Inline Stream Monitoring Phase
- Stream the upstream response through, unbuffered.
- **The stream is NOT cut when it crosses the balance.** `mid_stream_cutoff = false`
  is deliberate (`config/apikita.toml`): a truncated answer on non-refundable funds
  is the most likely source of a delivery dispute. The hold simply bounds the loss.

Phase 3: Post-Stream Final Settlement Phase
- Read the usage block the provider reports (or parse it from the final SSE chunk).
- Compute the true cost with `ceil()` at the per-model multiplier.
- Settle against the **embedded SQLite wallet in the same transaction** as the
  ledger rows — there is no Redis (`server/src/routes/proxy.rs`), and the release
  of the unused reserved balance lands with the debit, not after it.

--------------------------------------------------------------------------------

3. TECHNICAL ARCHITECTURE & SYSTEM TOPOGRAPHY

3.1 Network Topology
To circumvent cross-border packet dropping and filtering rules enforced by state 
firewalls, the Rust proxy binary is strategically deployed to edge nodes in 
Hong Kong or Singapore. These locations maintain dedicated high-speed fiber 
backbones directly interconnected with mainland networks, compressing connection 
handshakes and stabilizing streaming performance.

3.2 Thread-Safe Engine Structs
The system abstracts raw supplier configurations behind an immutable public
routing layer. Configuration is read from standard TOML files **once at startup**
and held shared and immutable for the process's life as `Arc<AppConfig>`
(`server/src/routes/proxy.rs` `AppState`).

**It is NOT hot-reloadable.** The `Arc<RwLock<T>>` described here is the design
sketch, not the build: a config change is a **restart**, not an in-place reload.
(The settle path does defend against a reload it cannot currently experience —
a settled model that is no longer in the config releases its hold rather than
stranding it — but that is a guard, not a feature.)

[CODE STRUCTURE]
pub struct TokenRates {
    pub input_cost_per_m: f64,
    pub output_cost_per_m: f64,
    pub cache_read_cost_per_m: f64,
}

pub struct EndpointConfiguration {
    pub id: String,
    pub url: String,
    pub upstream_model_name: String, 
    pub api_key: String,
    pub rates: TokenRates,
    pub is_healthy: bool,
}

pub struct ModelMapping {
    pub public_name: String,         
    pub endpoints: Vec<EndpointConfiguration>,
}

pub struct ConfigWrapper {
    pub models: Vec<ModelMapping>,
}

3.3 Multi-Endpoint Resolution
When a client payload arrives requesting a model abstraction, the router resolves it
to concrete upstreams and tries them **in configured order**, skipping any endpoint
that is weight-0 (unregistered terms) or whose circuit is open. The payload's model
name is rewritten to the endpoint's own upstream model name per attempt.

**There is no "randomized or weighted" balancing.** The design sketch here promised
inline load-balancing mutation across active vectors; the build is a deterministic
failover loop, and that is deliberate — it makes behaviour reproducible and the
failover order auditable (`server/src/upstream/client.rs`, `[models.endpoints]`
weight semantics in `config/apikita.toml`).

--------------------------------------------------------------------------------

4. OPERATIONAL PIPELINE & HIGH AVAILABILITY

4.1 Zero-Copy Reverse Proxy Streaming
To sustain extreme volumes of token traffic on minimal host hardware, the proxy 
avoids loading full HTTP bodies into RAM. Utilizing stream abstractions from 
the axum and reqwest libraries, the engine pipes incoming network segments from 
the supplier socket directly out to the customer socket without intermediate buffer allocations.

4.2 Non-Blocking Background Accounting
To optimize Time-To-First-Token (TTFT), tracking and updating client billing 
balances occurs asynchronously. The system extracts the metadata trailer 
embedded in the final API stream snippet and offloads the calculation logic 
to independent tokio::spawn worker pools. 

The financial accounting evaluates the true prompt cache hit ratio to prevent deficit generation:
User Invoice (IDR) = 1.50 * [ (Raw_Input / 10^6 * R_input) + (Cached_Input / 10^6 * R_cache) + (Output / 10^6 * R_output) ]

4.3 Fault Tolerance & Circuit Breaking
The runtime framework integrates automated circuit breaking middleware. If an 
active upstream endpoint exhibits recurrent networking anomalies (>3 consecutive 
5xx errors or network timeouts), the orchestrator marks the provider as 
is_healthy = false inside the active shared state. Traffic seamlessly shifts 
to secondary fallback channels instantly, maintaining uninterrupted uptimes 
for the terminal client application.
================================================================================
