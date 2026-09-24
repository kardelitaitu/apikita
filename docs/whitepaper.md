================================================================================
WHITEPAPER: CO-PLUS LLM ARBITRAGE ROUTING ENGINE
A High-Throughput, Risk-Free Arbitrage Proxy Gateway Built in Rust
================================================================================

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
  * Wholesale Cost (Base): ~1,100 IDR / 1M
  * Consumer Price (+50%): 1,650 IDR / 1M
  * Net Arbitrage Margin:  +550 IDR / 1M

- Model Output Tokens:
  * Wholesale Cost (Base): ~4,400 IDR / 1M
  * Consumer Price (+50%): 6,600 IDR / 1M
  * Net Arbitrage Margin:  +2,200 IDR / 1M

- Cache Read (KV Hit) Tokens:
  * Wholesale Cost (Base): ~22 IDR / 1M
  * Consumer Price (+50%): 33 IDR / 1M
  * Net Arbitrage Margin:  +11 IDR / 1M

Even with a uniform 50% markup, the consumer rate remains highly competitive 
relative to standard retail API entry points, providing a dual value proposition: 
deep cost savings for the customer and guaranteed profitability for the operator.

2.2 Risk Elimination Matrix
To safely offer variable, post-request token billing under a prepaid architecture, 
the system operates a strict three-phase verification lifecycle:

Phase 1: Pre-Flight Reservation Phase
- Parse input text length using `tiktoken-rs`
- Compute Worst-Case Cost (0% Cache + Max Output tokens)
- If Wallet Balance < Worst-Case Cost -> REJECT request immediately.

Phase 2: Inline Stream Monitoring Phase
- Pipe network buffers via zero-copy tokio sockets
- If generated response tokens cross remaining balance mid-sentence -> CUT pipeline.

Phase 3: Post-Stream Final Settlement Phase
- Extract actual usage block from the final JSON chunk sent by provider
- Compute true cost with strict 50% multiplier applied
- Deduct final cost from Redis wallet and release unused reserved balance.

--------------------------------------------------------------------------------

3. TECHNICAL ARCHITECTURE & SYSTEM TOPOGRAPHY

3.1 Network Topology
To circumvent cross-border packet dropping and filtering rules enforced by state 
firewalls, the Rust proxy binary is strategically deployed to edge nodes in 
Hong Kong or Singapore. These locations maintain dedicated high-speed fiber 
backbones directly interconnected with mainland networks, compressing connection 
handshakes and stabilizing streaming performance.

3.2 Thread-Safe, Hot-Reloadable Engine Structs
The system abstracts raw supplier configurations behind an immutable public 
routing layer. Configuration changes are executed via standard TOML files 
parsed dynamically in memory using thread-safe read/write primitives (Arc<RwLock<T>>).

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

3.3 Dynamic Multi-Endpoint Resolution
When a client payload arrives requesting a generic model abstraction (e.g., 
"model": "model_a"), the router intercepts the request, isolates healthy suppliers, 
executes an inline load-balancing mutation, and alters the network payload 
dynamically using randomized or weighted algorithms across available active vectors.

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
