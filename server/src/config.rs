use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AppConfig {
    pub pricing: PricingConfig,
    pub wallet: WalletConfig,
    pub sessions: SessionsConfig,
    /// Identity mechanics: how a password is hashed, what a password must be,
    /// and how long a verification or reset link stays valid.
    ///
    /// Placed after `sessions` because the field order here is the TOML section
    /// order and a reader of `config/apikita.toml` meets `[auth]` right after
    /// `[sessions]`. The rate limits that bound these endpoints are NOT here:
    /// they live in `[limits]`, which already owns every per-hour cap.
    ///
    /// `serde(default)` because this section was added after the first
    /// deployments' configs were written, and a config that predates it must
    /// still BOOT - a `[auth]`-less file is a config from before identity was
    /// native, not a broken one. `AuthConfig::default` carries the SHIPPED
    /// values, so the defaults are not a second set of numbers to keep in step.
    #[serde(default)]
    pub auth: AuthConfig,
    /// Outbound transactional mail: the SMTP relay that carries verification
    /// and reset links. A separate section because it is a separate transport
    /// with a separate failure mode - see the `smtp_host` field of
    /// `EmailConfig`.
    ///
    /// `serde(default)` for the same reason as `auth`: an older config has no
    /// `[email]`, and the default is "no relay configured", which is a state
    /// signup tolerates rather than a crash.
    #[serde(default)]
    pub email: EmailConfig,
    pub limits: LimitsConfig,
    pub realtime: RealtimeConfig,
    pub key_pool: KeyPoolConfig,
    pub circuit_breaker: CircuitBreakerConfig,
    pub streaming: StreamingConfig,
    pub network: NetworkConfig,
    pub models: Vec<ModelConfig>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct NetworkConfig {
    /// CIDRs of reverse proxies allowed to speak for the caller.
    ///
    /// Only a peer inside one of these networks has its `X-Forwarded-For`
    /// consulted; from anywhere else the header is ignored and the TCP peer is
    /// recorded. `docs/ip-tracking.md` — the header is client-controlled, and
    /// trusting it blindly lets a caller pin its own recorded address.
    pub trusted_proxy_cidrs: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PricingConfig {
    pub currency: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct WalletConfig {
    pub min_topup: u64,
    pub min_first_deposit: u64,
    pub min_monthly_tokens: u64,
    pub dormancy_days: u32,
    pub reserve_settlement_cycles: u32,
    pub low_balance_threshold_idr: u64,
    pub low_balance_max_per_day: u32,
    /// How long a deposit's credit stays spendable, in months, counted from that
    /// deposit's own settlement date.
    ///
    /// Read by `db::credit_topup_transaction`, which stamps the resulting
    /// `topups.credit_expires_at`, and by `db::expire_credit`, which retires a
    /// deposit once the instant passes. Both read it from this one field, so the
    /// stamp and the sweep cannot be stated over different windows.
    ///
    /// PER DEPOSIT, not per account and not from last activity. The distinction
    /// is the whole policy (`docs/decisions.md:76`): crediting time back on every
    /// spend would mean a customer who keeps using the service never has anything
    /// expire, which is the opposite of what the terms promise.
    ///
    /// `serde(default)` because this key was added after the first configs were
    /// written, and a config that predates it must still boot. 0 disables expiry,
    /// which leaves the terms promising something the code does not do - so it is
    /// a development setting, not an operator choice.
    #[serde(default = "default_credit_expiry_months")]
    pub credit_expiry_months: u32,
}

/// The shipped credit lifetime. Also the value in `config/apikita.toml`, so a
/// config that omits the key behaves like one that states it.
fn default_credit_expiry_months() -> u32 {
    24
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SessionsConfig {
    pub absolute_days: u32,
    pub idle_days: u32,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AuthConfig {
    /// Argon2id memory cost, KiB. The dominant knob, and the one to raise first.
    ///
    /// Read on EVERY hash and EVERY verify
    /// (`identity::password::hash_password` and `::verify_password`), so a value
    /// changed here changes the work factor for new hashes immediately and for
    /// the verification of an existing hash on the next sign-in. 19456 KiB
    /// (19 MiB) with 2 passes is the OWASP floor for Argon2id and RFC 9106's
    /// second recommended option; raising it multiplies sign-in latency.
    pub argon2_memory_kib: u32,
    /// Argon2id time cost, in passes over memory.
    pub argon2_iterations: u32,
    /// Argon2id lanes. Argon2 DIVIDES by this, so the validator refuses 0.
    pub argon2_parallelism: u32,
    /// A FLOOR, not a composition rule: no character-class requirement, because
    /// a length floor is the only password rule that measurably helps.
    ///
    /// Read by `identity::password::validate_password`, which is the only gate
    /// on both signup and reset, so the two paths cannot drift. The signup page
    /// states this same number to the user, so the validator refuses a value
    /// below 8 - a server floor under the published one would make the page
    /// describe a rule we do not enforce.
    pub password_min_length: usize,
    /// Bounds the Argon2 INPUT, not the cost.
    ///
    /// Argon2's cost is set by its parameters, not by the input length, so a
    /// 1 MiB "password" buys an attacker nothing and costs us the copy and the
    /// hash of it. Without this bound the endpoint is a free CPU burn.
    pub password_max_length: usize,
    /// How long a verification link stays usable.
    ///
    /// Read by `identity::tokens::issue`, which computes `expires_at`, and by
    /// `::consume`, which refuses a row past it. Generous on purpose: the user
    /// may open the mail tomorrow.
    pub verification_ttl_minutes: u32,
    /// How long a password-reset link stays usable. Short on purpose, and the
    /// reset page says so to the user.
    pub reset_ttl_minutes: u32,
    /// The audience a Google ID token must name. Not a secret, so it is config
    /// rather than an environment variable. Read by
    /// `identity::google::verify_id_token`.
    pub google_client_id: String,
    /// How long a fetched Google JWKS is reused before refetching.
    ///
    /// Read by `identity::google::jwks`. Without a cache every sign-in is an
    /// outbound request to Google, which makes Google's availability our
    /// sign-in latency and our sign-in failure mode.
    pub google_jwks_cache_seconds: u64,
}

/// The SMTP relay that carries a verification or reset link.
///
/// **SMTP, not a vendor's HTTP API.** The decision (docs/decisions.md, the
/// identity row) is a generic seam so the launch credentials do not pick the
/// architecture: an operator points this at whatever relay they already have,
/// and swapping relays is four config values rather than a code change. A
/// vendor-shaped client would have made the vendor part of the product.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct EmailConfig {
    /// Hostname of the SMTP relay. `""` means NO provider is configured.
    ///
    /// The empty string is a supported, deliberate state rather than an error:
    /// signup still works without mail (the account is created unverified, which
    /// is inert - it cannot hold a balance), so a deployment without a relay is
    /// not a deployment that cannot register customers. Read by
    /// `identity::email::EmailSender::new` through `listen_address`, which also
    /// lets `APIKITA_EMAIL_BASE_URL` override it.
    pub smtp_host: String,
    /// SMTP submission port. 587 is submission with STARTTLS; 465 is implicit
    /// TLS. Both are accepted because relays differ. Read by
    /// `identity::email::EmailSender::new` through `listen_address`.
    pub smtp_port: u16,
    /// SMTP username. Empty means the relay takes no authentication, which is
    /// the norm for a relay on the same private network.
    pub smtp_username: String,
    /// NAME of the environment variable holding the SMTP password - never the
    /// password. Same convention as the file header rule and `api_key_envs` on
    /// the model endpoints. Read by `identity::email::EmailSender::new`
    /// through `std::env::var`.
    pub smtp_password_env: String,
    /// The envelope sender. Read by `identity::email::EmailSender::new`.
    pub from_address: String,
    /// The display name on the From header. Read by
    /// `identity::email::EmailSender::new`.
    pub from_name: String,
    /// Read by `identity::email::EmailSender::new` when non-empty, and carried on
    /// every message it builds.
    pub reply_to: String,
    /// The connect/IO timeout, read by `identity::email::EmailSender::new`
    /// through `timeout_of`, which floors it so a `0` cannot mean "wait forever".
    pub request_timeout_seconds: u64,
}

// THE CITATIONS ABOVE WERE WRONG IN TWO WAYS, and both are worth recording so
// the shape is not reintroduced.
//
// First, they named a constructor `from_config` on `EmailSender` - a method that
// does not exist, and never did. The constructor is `new`
// (server/src/identity/email.rs:166). A doc-comment
// naming a function nobody can find is worse than one naming none: it reads as
// a verified cross-reference, so a reader who wants to know where a config field
// is consumed follows it, finds nothing, and concludes the field is unused -
// which is the exact question these comments exist to answer.
//
// Second, `from_address`, `from_name` and `reply_to` said `EmailSender::send`.
// That is wrong too: `send` takes an already-built `Email` and reads no config
// at all. All three are read in `new`, which parses the addresses once at
// startup - `reply_to` at email.rs:210 and `from_address`/`from_name` at
// email.rs:167-179 - so a rename here needs `new` looked at, not `send`.
//
// The citations are now the method that actually reads each field. Nothing
// enforced them before; see the guard noted in `docs/architecture/`.

/// The values `[auth]` has when a config file does not name them.
///
/// These are the SHIPPED values from `config/apikita.toml`, stated once here and
/// once there - deliberately, because the TOML is the operator's copy and this
/// is the fallback for a config that predates the section. `Default` rather than
/// per-field `#[serde(default = "fn")]` so the whole section has one home: an
/// all-or-nothing default cannot half-apply, which a per-field set can (a
/// section present with one key missing would take the file's value for the key
/// and this default for the rest, and the two would not be recognisable as a
/// pair).
impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            argon2_memory_kib: 19456,
            argon2_iterations: 2,
            argon2_parallelism: 1,
            password_min_length: 8,
            password_max_length: 128,
            verification_ttl_minutes: 1440,
            reset_ttl_minutes: 30,
            google_client_id: String::new(),
            google_jwks_cache_seconds: 3600,
        }
    }
}

/// `smtp_host` empty IS the default, and it means "no relay configured".
///
/// That is the safe direction for a config that predates the section: a
/// deployment that has not configured mail does not start sending, and the
/// signup path tolerates it by creating an unverified - inert - account rather
/// than refusing the request.
impl Default for EmailConfig {
    fn default() -> Self {
        Self {
            smtp_host: String::new(),
            smtp_port: 587,
            smtp_username: String::new(),
            smtp_password_env: String::new(),
            from_address: String::new(),
            from_name: "apikita".to_string(),
            reply_to: String::new(),
            request_timeout_seconds: 10,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LimitsConfig {
    pub topup_per_hour: u32,
    pub wallet_mutations_per_minute: u32,
    pub key_creation_per_day: u32,
    pub review_per_hour: u32,
    /// How many link codes one account may have ISSUED within the hour.
    ///
    /// Bounds codes in flight per account. `0` disables, the project-wide
    /// convention every other ceiling here follows.
    ///
    /// `serde(default)` because this key was added after the first deployments'
    /// configs were written, and a config that predates it must still BOOT. The
    /// default is the SAFE one - a missing key leaves the cap ON rather than
    /// silently off, so forgetting it fails closed.
    #[serde(default = "default_link_code_issuance_per_hour")]
    pub link_code_issuance_per_hour: u32,
    /// How many redemption ATTEMPTS one client IP may make within the hour.
    ///
    /// This is the cap that actually stops a brute-force: a 6-digit code is 10^6
    /// possibilities and a guesser learns nothing from a refusal, so the endpoint's
    /// safety is this number, not the code's secrecy
    /// (docs/architecture/identity.md - "the highest-risk endpoint"). FAILED
    /// attempts count, which is the only way it can ever fire. `0` disables, and
    /// `serde(default)` applies the same reasoning as the field above: an older
    /// config boots with the cap still ON.
    #[serde(default = "default_link_redemption_per_hour")]
    pub link_redemption_per_hour: u32,
    /// How many sign-in ATTEMPTS one client IP may make within the hour.
    ///
    /// A sign-in is the one endpoint where an attacker controls the secret being
    /// guessed, so a password list is only bounded by a count. This cap is per IP
    /// and the one below is per ACCOUNT on purpose: they fail differently. A
    /// distributed guesser defeats the IP cap without noticing it, and a guesser
    /// walking a single account from one address is better caught by the account
    /// cap, which no amount of proxying dilutes. FAILED attempts count - a cap
    /// that only counts successes never fires. `0` disables, the project-wide
    /// convention; `serde(default)` keeps an older config's cap ON.
    #[serde(default = "default_login_per_hour_per_ip")]
    pub login_per_hour_per_ip: u32,
    /// How many sign-in ATTEMPTS may be made against ONE ACCOUNT within the hour,
    /// from any address. See the field above for why both exist.
    ///
    /// Deliberately SMALLER than the per-IP cap: a human signing in normally needs
    /// one or two, so a dozen is already generous, and the smaller the number the
    /// less useful a leaked password list is. It is also the cap that makes an
    /// account-enumeration sweep by email useless, because the refusal arrives at
    /// the same speed whether or not the address exists.
    #[serde(default = "default_login_per_hour_per_account")]
    pub login_per_hour_per_account: u32,
    /// How many accounts one client IP may CREATE within the hour.
    ///
    /// Signup is unauthenticated and sends mail, so it is the one place a caller
    /// can spend our money and our sending reputation without an account. FIVE is
    /// chosen against the real need: a household or an office behind one address
    /// shares an IP, and a shared NAT is the case that must not break. `0`
    /// disables; `serde(default)` keeps the cap ON for an older config.
    #[serde(default = "default_signup_per_hour_per_ip")]
    pub signup_per_hour_per_ip: u32,
    /// How many PASSWORD RESET requests one ACCOUNT may make within the hour.
    ///
    /// Resets send mail, so this bounds both the mail bill and the use of the
    /// reset endpoint as a way to flood somebody else's inbox. THREE an hour is
    /// well beyond a person who mistyped their address twice. Per account, not per
    /// IP, because the resource being protected is the victim's mailbox.
    #[serde(default = "default_password_reset_per_hour_per_account")]
    pub password_reset_per_hour_per_account: u32,
    /// How many VERIFICATION EMAILS one ACCOUNT may be sent within the hour.
    ///
    /// The resend control is a button a user can hold down, and every press is a
    /// message we pay for and a chance to be marked as a sender of unwanted mail.
    /// `0` disables; `serde(default)` keeps the cap ON.
    #[serde(default = "default_verification_resend_per_hour")]
    pub verification_resend_per_hour: u32,
    pub key_metadata_cache_seconds: u64,
}

/// Default for the `login_per_hour_per_ip` field of `LimitsConfig` when a config file predates
/// the key.
///
/// Twenty an hour. A person who has forgotten which password they used hits two
/// or three; a list-driven attack wants thousands.
fn default_login_per_hour_per_ip() -> u32 {
    20
}

/// Default for the `login_per_hour_per_account` field of `LimitsConfig` when a config file
/// predates the key. Half the per-IP cap, and deliberately so - see the field.
fn default_login_per_hour_per_account() -> u32 {
    10
}

/// Default for the `signup_per_hour_per_ip` field of `LimitsConfig` when a config file predates
/// the key. Five an hour: room for one office or household, not for a farm.
fn default_signup_per_hour_per_ip() -> u32 {
    5
}

/// Default for the `password_reset_per_hour_per_account` field of `LimitsConfig` when a config
/// file predates the key.
fn default_password_reset_per_hour_per_account() -> u32 {
    3
}

/// Default for the `verification_resend_per_hour` field of `LimitsConfig` when a config file
/// predates the key.
fn default_verification_resend_per_hour() -> u32 {
    3
}

/// Default for the `link_code_issuance_per_hour` field of `LimitsConfig` when a config file
/// predates the key. Ten codes an hour is far more than a human needs and far
/// less than a code farm wants.
fn default_link_code_issuance_per_hour() -> u32 {
    10
}

/// Default for the `link_redemption_per_hour` field of `LimitsConfig` when a config file
/// predates the key.
///
/// Twenty attempts an hour, matching the shipped `config/apikita.toml`. It is a
/// DELIBERATE, SMALL number, and the arithmetic is worth stating because it is easy
/// to get wrong by three orders of magnitude: at 20/h a host gets ~1.7 guesses
/// inside one 5-minute code window, so the expected time to hit a *specific*
/// account's live code is ~5.7 years (1/(1.7e-6) windows), NOT 5,700 years. The
/// reason it holds is the TTL plus the one-live-code-per-account rule: the code
/// rotates, so the 10^6 space never shrinks and the search cannot be amortised.
/// A real user mistyping a code hits a handful of these.
fn default_link_redemption_per_hour() -> u32 {
    20
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RealtimeConfig {
    pub replay_buffer_events: usize,
    pub max_connections_per_account: usize,
    pub max_stream_seconds: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct KeyPoolConfig {
    pub rate_limit_status: Vec<u16>,
    pub key_cooldown_seconds: u64,
    pub max_key_attempts: usize,
    pub on_pool_exhausted: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CircuitBreakerConfig {
    pub failure_threshold: u32,
    pub cooldown_seconds: u64,
    pub cooldown_max_seconds: u64,
    pub cooldown_multiplier: f64,
    pub request_timeout_seconds: u64,
    pub health_check_interval_seconds: u64,
    pub health_check_failures: u32,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct StreamingConfig {
    pub mid_stream_cutoff: bool,
    pub hard_max_output_tokens: u64,
    /// NOT A CAP. Nothing refuses a request whose prompt exceeds this, and the hold is
    /// sized from the ACTUAL body (`estimated_input_tokens`), so an over-long prompt is
    /// still covered - the ceiling simply bounds nothing. It is carried because the
    /// wholesale price lists publish it and an operator reading a model page will look
    /// for it; it is RECORDED, NOT ENFORCED. See the UNWIRED register in the tests
    /// below and the "Not enforced" row in docs/decisions.md.
    pub max_context_tokens: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ModelConfig {
    pub name: String,
    pub description: String,
    pub price: f64,
    /// NOT A CAP - see [the `max_context_tokens` field of `StreamingConfig`]. This is the MODEL's
    /// published context window, carried so the config matches the price list it was
    /// transcribed from. Neither this nor the streaming ceiling is ever compared
    /// against a prompt.
    pub max_context_tokens: u64,
    pub max_output_tokens: u64,
    pub supports_vision: bool,
    pub supports_thinking: bool,
    pub billing_basis: String,
    pub rates: ModelRates,
    #[serde(default)]
    pub endpoints: Vec<ModelEndpoint>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ModelRates {
    pub cache_read_offpeak: f64,
    pub cache_read_peak: f64,
    pub input_offpeak: f64,
    pub input_peak: f64,
    pub output_offpeak: f64,
    pub output_peak: f64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ModelEndpoint {
    pub name: String,
    pub url: String,
    pub upstream_model: String,
    #[serde(default)]
    pub api_key_envs: Vec<String>,
    #[serde(default)]
    pub concurrency_per_key: usize,
    #[serde(default)]
    pub weight: f64,
    #[serde(default)]
    pub supports_stream_options: bool,
    /// Peak input rate for THIS endpoint, IDR per 1M tokens. `None` means the
    /// model's rate.
    ///
    /// Upstream cost is a property of the PROVIDER, not of the product: two
    /// endpoints of one model are the same thing sold at the same price, bought
    /// from resellers who charge differently. The pre-flight reservation is taken
    /// before routing and may be served by any endpoint in the pool, so it must
    /// cover the DEAREST one (docs/failover.md (Ordering with the wallet checks)); without a per-endpoint
    /// rate there is nothing for that rule to compare.
    #[serde(default)]
    pub input_peak: Option<f64>,
    /// Peak output rate for THIS endpoint, IDR per 1M tokens. See `input_peak`.
    #[serde(default)]
    pub output_peak: Option<f64>,
}

impl ModelEndpoint {
    /// This endpoint's peak input rate: its own when it overrides, else the
    /// model's. One definition, because the reservation and the flattened
    /// upstream client must resolve an override the same way or they drift.
    pub fn effective_input_peak(&self, model: &ModelConfig) -> f64 {
        self.input_peak.unwrap_or(model.rates.input_peak)
    }

    /// This endpoint's peak output rate: its own when it overrides, else the
    /// model's. See `effective_input_peak`.
    pub fn effective_output_peak(&self, model: &ModelConfig) -> f64 {
        self.output_peak.unwrap_or(model.rates.output_peak)
    }
}

impl ModelConfig {
    /// THE PRE-FLIGHT RESERVATION RULE, in ONE place.
    ///
    /// The hold is taken BEFORE routing and applies whichever endpoint serves the
    /// request, so it must cover the DEAREST endpoint in the pool — a failover to
    /// a dearer provider would otherwise overdraw the balance
    /// (docs/failover.md (Ordering with the wallet checks)). Each endpoint is priced at its own peak rates
    /// when it overrides them, else the model's; a model with no endpoints
    /// registered reserves at the model rate.
    ///
    /// This is the ONLY implementation. There was a second, on `UpstreamClient`,
    /// which proxied to a `ModelEntry` copy of the same rule - and the handler and
    /// that copy had each carried their own version, so the comment above used to
    /// present them as parallel. The copy was called by NOTHING outside its own
    /// tests: the handler has always called this one. A second implementation of a
    /// money rule that no request reaches is worse than none, because it reads as a
    /// cross-check and is not one - and a bug fixed in this copy in the same commit
    /// that fixed a hold under-reserving left the dead copy still wrong, with tests
    /// green over both.
    ///
    /// So it is deleted rather than left as a spare, and the fields it alone read
    /// went with it. The rule now has one home, and the only test of it is a test of
    /// the code that actually runs.
    ///
    /// AND IT IS NOT JUST A SPARE THAT IS DELETED - it is also that the OUTPUT side of
    /// the hold had the same problem in reverse. `max(requested, model cap).min(hard
    /// cap)` was written inline in the handler, and then REWRITTEN a second time in two
    /// test helpers, because a test could not call an expression buried in the middle
    /// of a request. So the formula the money depended on was pinned in two copies
    /// that no test shared, and neither was the line the handler actually runs.
    ///
    /// This method is that formula, so the three sites call ONE implementation and the
    /// tests call the code that actually runs.
    ///
    /// The output tokens the pre-flight hold is taken against, and WHY it is not the
    /// client's own cap.
    ///
    /// A request that omits `max_tokens`, or asks for less than the model can produce,
    /// still lets the upstream stream up to the model's own ceiling - so holding only
    /// the client's figure leaves a 4096-token hold guarding a model that can emit
    /// 384000 tokens, and an over-long answer overdrew the wallet. The bound is
    /// therefore max(requested, model cap), clamped to the global hard cap.
    ///
    /// The tradeoff is deliberate and worth stating, because it takes money from
    /// clients who asked for less: when a client asks for 1000 tokens the hold covers
    /// the model's 384000. The true cost is charged at settlement regardless, so the
    /// hold is released down to what was actually used - over-asking customers lose
    /// headroom, and the under-reserved case is gone entirely. A hold's job is to
    /// never under-reserve, and an over-reserve is bounded and refunded.
    ///
    /// The clamp is NOT vacuous, which is worth saying because it reads that way when
    /// `hard_max_output_tokens` and every model's `max_output_tokens` ship at the same
    /// 384000. The two fire in different places: the model cap binds for every
    /// ordinary request, and the HARD cap binds only when a client ASKS for more -
    /// `{"max_tokens": 1000000}` clamps to 384000. Setting the global equal to the
    /// model cap is therefore a backstop, not a no-op; raising a model's ceiling above
    /// the global is what makes the global the binding constraint for everyone.
    pub fn reserved_output_tokens(&self, requested: Option<u64>, hard_cap: u64) -> u64 {
        requested
            .unwrap_or(0)
            .max(self.max_output_tokens)
            .min(hard_cap)
    }
    pub fn worst_case_reservation_idr(
        &self,
        estimated_input_tokens: u64,
        max_output_tokens: u64,
    ) -> i64 {
        let at = |input_peak: f64, output_peak: f64| {
            crate::money::calculate_preflight_reservation_idr(
                self.price,
                estimated_input_tokens,
                input_peak,
                max_output_tokens,
                output_peak,
            )
        };

        // THE MODEL'S OWN RATES ARE A CANDIDATE, not merely the empty-pool
        // fallback, and that is the whole fix.
        //
        // Two rate sets are in play and both are deliberate. This hold is taken
        // BEFORE routing, so it prices each endpoint at its own rates and keeps the
        // dearest - that is why per-endpoint rates exist at all. SETTLEMENT, by
        // contrast, always charges the MODEL's rates: every call site
        // (routes/proxy.rs:1522, :2637, :3331) passes model_cfg.rates.* and never
        // the endpoint's, because once the answer is streamed the customer pays the
        // product price, not the reseller's.
        //
        // Taking the max over ENDPOINTS alone therefore did not guarantee the one
        // thing the hold has to do: cover the charge. The model's rates were only
        // ever a fallback for an empty pool, so an endpoint that overrides DOWNWARD
        // - the ordinary reason to add an override, this reseller is cheaper -
        // pulled the hold below the model-rate charge. The result is a stranded
        // hold: a -cost larger than the hold, a negative balance, and a reconcile
        // query that is structurally blind to it. Invisible money.
        //
        // Seeding the fold with the model-rate hold states the invariant directly:
        // the product price is the FLOOR, and an endpoint may only raise it.
        //
        // With no overrides - every config that ships today, including
        // config/apikita.toml - the endpoints already tie with the model, so the
        // result is unchanged and the feature stays behaviour-preserving.
        let at_model_rate = at(self.rates.input_peak, self.rates.output_peak);
        self.endpoints
            .iter()
            .map(|endpoint| {
                at(
                    endpoint.effective_input_peak(self),
                    endpoint.effective_output_peak(self),
                )
            })
            .fold(at_model_rate, i64::max)
    }
}

impl AppConfig {
    pub fn load_from_file<P: AsRef<Path>>(path: P) -> Result<Self, Box<dyn std::error::Error>> {
        let content = fs::read_to_string(path)?;
        let config: AppConfig = toml::from_str(&content)?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<(), Box<dyn std::error::Error>> {
        // A malformed trust rule is a security decision, so it is rejected here
        // rather than skipped at runtime: see `parse_cidrs`.
        crate::ip_tracking::parse_cidrs(&self.network.trusted_proxy_cidrs)
            .map_err(|err| format!("network.trusted_proxy_cidrs: {err}"))?;
        validate_trusted_proxy_width(&self.network.trusted_proxy_cidrs)?;

        // The LAST f64 in this config, and the one that was already defended at its
        // use site: next_cooldown in upstream/circuit_breaker.rs checks
        // is_finite() and falls back to 1.0. That fallback is safe but SILENT - a
        // breaker whose backoff never grows is a breaker that retries a failing
        // provider at full rate, and nothing anywhere says the operator's
        // multiplier was ignored. Every other f64 here is now refused at load, so
        // refusing this one too is what makes the rule a rule rather than a habit.
        //
        // A multiplier of 1.0 or less is legal and documented as "disables the
        // backoff", so only finiteness is refused - the positivity case stays with
        // the circuit breaker, which is where that semantic already lives.
        if !self.circuit_breaker.cooldown_multiplier.is_finite() {
            return Err(format!(
                "circuit_breaker.cooldown_multiplier is not a finite number: {}",
                self.circuit_breaker.cooldown_multiplier
            )
            .into());
        }

        // COOLDOWNS ARE ADDED TO AN INSTANT, AND THAT PANICS ON OVERFLOW.
        //
        // Three settings reach `Instant + Duration` on the request path: the
        // breaker's base cooldown and its cap, and the key pool's cooldown. Unlike
        // the f64 fields above, nothing here saturates - a bare `+` on an Instant
        // that leaves its representable range is a PANIC, on a request, from a
        // thread holding the breaker lock.
        //
        // MEASURED, not assumed, because the obvious guess is wrong in the
        // reassuring direction:
        //
        //     Instant::now() + Duration::from_secs(1_000_000_000_000)  ok
        //     Instant::now() + Duration::from_secs(u64::MAX)            PANIC
        //
        // A trillion seconds is 31,700 years and does not panic. So this is NOT a
        // live bug and the commit that added it says so: a config typo would have
        // to be a nineteen-digit number to reach it. It is here because when such a
        // number does appear, the alternative is a panic minutes later in a
        // circuit breaker, on a request, with nothing in the log naming the setting
        // that caused it. Refusing it at load names the field and the value.
        //
        // `checked_add` rather than a magic ceiling, so the rule is the actual
        // property instead of a number this file would have to defend. It is
        // evaluated against the clock at load, which is conservative for a service
        // that then runs for years.
        //
        // ONLY THESE THREE. `max_stream_seconds` and `request_timeout_seconds` are
        // also large numbers from the same config, but they reach `tokio::time`,
        // which saturates rather than panicking - bounding them would be a rule
        // with no failure behind it.
        for (field, secs) in [
            (
                "circuit_breaker.cooldown_seconds",
                self.circuit_breaker.cooldown_seconds,
            ),
            (
                "circuit_breaker.cooldown_max_seconds",
                self.circuit_breaker.cooldown_max_seconds,
            ),
            (
                "key_pool.key_cooldown_seconds",
                self.key_pool.key_cooldown_seconds,
            ),
        ] {
            if Instant::now()
                .checked_add(Duration::from_secs(secs))
                .is_none()
            {
                return Err(format!(
                    "{field} = {secs} cannot be a cooldown: adding it to the clock \
                     overflows, and the circuit breaker and key pool would PANIC on a \
                     request rather than refuse one"
                )
                .into());
            }
        }

        // SESSION LIFETIMES ARE ADDED TO A DateTime, AND THAT PANICS ON OVERFLOW TOO.
        //
        // The same class as the cooldowns above, with a DIFFERENT boundary, which is
        // why it is checked separately rather than folded into the same loop: chrono's
        // NaiveDate and std's Instant have unrelated ranges. Measured again rather
        // than assumed, and the reassuring direction holds here as well:
        //
        //     now + Duration::days(10_000_000)    ok     (27,000 years)
        //     now + Duration::days(100_000_000)  PANIC  (273,000 years)
        //
        // Two settings reach it on the request path: `absolute_days`, which seeds
        // `expires_at` at login, and `idle_days`, which every session check adds to
        // `last_seen_at`. Both are u32, so a nine-digit value is enough - and a
        // nine-digit number is a far more plausible typo than the nineteen digits the
        // cooldown check needs. That is the whole difference between this and the
        // block above, and it is why this one is closer to a live bug.
        //
        // `checked_add_signed` for the same reason as `checked_add` above: the rule
        // is the property, not a number this file would have to defend.
        //
        // `idle_days` is the one that hurts. The absolute bound is evaluated once, at
        // login; the idle bound is added on EVERY session resolution, so a value that
        // panics would do so on the first request of every request thereafter.
        let now_utc = chrono::Utc::now();
        for (field, days) in [
            ("sessions.absolute_days", self.sessions.absolute_days),
            ("sessions.idle_days", self.sessions.idle_days),
        ] {
            if now_utc
                .checked_add_signed(chrono::Duration::days(i64::from(days)))
                .is_none()
            {
                return Err(format!(
                    "{field} = {days} cannot be a session lifetime: adding it to the \
                     clock overflows the representable date range, and the session \
                     check would PANIC on the request rather than refuse the session"
                )
                .into());
            }
        }

        // THE PASSWORD HASHER'S PARAMETERS ARE A SECURITY RULE, NOT A PREFERENCE.
        //
        // Argon2 refuses rather than degrades on an impossible parameter set, and
        // every one of these would produce a hasher that FAILS ON EVERY SIGN-IN -
        // a lockout with no symptom but "nobody can log in". Refusing the config
        // at load names the field; the failure mode if this did not exist is a
        // production login endpoint answering 500 for everyone.
        //
        // The minima are not style: 8 KiB is Argon2's own floor (it divides
        // memory by the block size), a zero iteration count is not a work factor,
        // and 0 lanes is a division by zero.
        if self.auth.argon2_memory_kib < 8 {
            return Err(format!(
                "auth.argon2_memory_kib = {} is below the Argon2 minimum of 8 KiB: \
                 hashing would fail on every sign-in rather than refuse one",
                self.auth.argon2_memory_kib
            )
            .into());
        }
        if self.auth.argon2_iterations == 0 {
            return Err("auth.argon2_iterations = 0 cannot be an iteration count: \
                        a single pass is not a work factor"
                .into());
        }
        if self.auth.argon2_parallelism == 0 {
            return Err("auth.argon2_parallelism = 0 cannot be a lane count: \
                        Argon2 divides by it"
                .into());
        }

        // THE PUBLISHED PASSWORD LENGTH IS A CONTRACT WITH THE SIGNUP PAGE.
        //
        // `website/src/lib/auth-flow.ts` states a minimum to the user, and a
        // server floor BELOW it means the page is describing a rule we do not
        // enforce - the page would be right about a stricter rule than exists.
        // The reverse (server stricter than published) is safe and legal, so only
        // the downward divergence is refused. Hardcoded 8 rather than imported:
        // the website is a separate package, and the number is stated in a doc
        // comment on both sides with the guard below pinning the pair.
        if self.auth.password_min_length < 8 {
            return Err(format!(
                "auth.password_min_length = {} is below 8: the signup page states 8 to \
                 the user, and a server floor below the published one means the page is \
                 describing a rule that is not enforced",
                self.auth.password_min_length
            )
            .into());
        }
        if self.auth.password_max_length < self.auth.password_min_length {
            return Err(format!(
                "auth.password_max_length = {} is below auth.password_min_length = {}: \
                 no password could satisfy both, so every signup would fail",
                self.auth.password_max_length, self.auth.password_min_length
            )
            .into());
        }

        // TOKEN LIFETIMES REACH THE SAME CLOCK THE SESSION LIFETIME DOES.
        //
        // Same class as the session block above, and checked the same way for the
        // same reason: `expires_at` is seeded by adding a duration to the clock
        // when a link is minted, and a bare `DateTime + Duration` PANICS on an
        // out-of-range result. A realistic typo here is a five-digit number of
        // minutes, which is nowhere near the boundary - but the failure if it were
        // reached is a panic on a public, unauthenticated endpoint.
        for (field, minutes) in [
            (
                "auth.verification_ttl_minutes",
                self.auth.verification_ttl_minutes,
            ),
            ("auth.reset_ttl_minutes", self.auth.reset_ttl_minutes),
        ] {
            if now_utc
                .checked_add_signed(chrono::Duration::minutes(i64::from(minutes)))
                .is_none()
            {
                return Err(format!(
                    "{field} = {minutes} cannot be a token lifetime: adding it to the \
                     clock overflows the representable date range, and minting a link \
                     would PANIC rather than refuse one"
                )
                .into());
            }
        }

        // AND SO DOES THE CREDIT EXPIRY.
        //
        // Same class again, and checked for the same reason: settlement adds
        // this span to the clock when it stamps `credit_expires_at`, and a bare
        // `DateTime + Months` PANICS on an out-of-range result. The realistic
        // typo is a months value with one digit too many - `2400` instead of
        // `24` - which is exactly far enough to reach the boundary, and the
        // failure if it were reached is a panic inside the Midtrans webhook:
        // the one endpoint where a crash means we cannot tell whether we were
        // paid.
        //
        // `checked_add_months` rather than `checked_add_signed`, because the
        // span is in calendar months and chrono clamps an end-of-month start
        // (31 January + 1 month is 28/29 February). Checking with a flat
        // duration would test a different arithmetic than the one that runs.
        if self.wallet.credit_expiry_months > 0
            && now_utc
                .checked_add_months(chrono::Months::new(self.wallet.credit_expiry_months))
                .is_none()
        {
            return Err(format!(
                "wallet.credit_expiry_months = {} cannot be an expiry span: adding it to \
                 the clock overflows the representable date range, and settling a deposit \
                 would PANIC rather than refuse one",
                self.wallet.credit_expiry_months
            )
            .into());
        }

        // AN UNCONFIGURED RELAY IS A STATE; A HALF-CONFIGURED ONE IS A BUG.
        //
        // `smtp_host = ""` is how a deployment says "no mail" and it is supported:
        // signup creates an inert, unverified account and the operator sees one
        // WARN at startup. But a host WITH no from-address, or a port of 0, is a
        // deployment that believes it sends mail and does not - every message
        // would be refused by the relay, and the first person to discover it is a
        // customer who never received a verification link.
        if !self.email.smtp_host.trim().is_empty() {
            if self.email.smtp_port == 0 {
                return Err("email.smtp_port = 0 while email.smtp_host names a relay: \
                            there is no port to connect to"
                    .into());
            }
            if self.email.from_address.trim().is_empty() {
                return Err("email.from_address is empty while email.smtp_host names a \
                            relay: every message would be rejected by the relay"
                    .into());
            }
        }

        if self.models.is_empty() {
            return Err("At least one model must be configured in models".into());
        }
        for model in &self.models {
            // FINITENESS FIRST, and it is a separate check rather than a wider
            // comparison because no comparison catches it. NaN <= 0.0 is FALSE in
            // IEEE 754 - NaN is unordered, so it is greater than nothing, less
            // than nothing, and equal to nothing - which means every "<= 0" guard
            // below waves a NaN straight through, and inf sails past them too.
            //
            // WHAT THIS IS AND IS NOT, measured rather than assumed. TOML does accept
            // "nan", "+inf" and "-inf" as float literals, so the VALUE is writable.
            // But the toml crate's deserializer refuses a non-finite float when it
            // reaches a struct field - "invalid type: floating point NaN, expected
            // struct CircuitBreakerConfig" - and load_from_file goes through exactly
            // that. So a config FILE cannot carry one into AppConfig today, and this
            // check is the SECOND line of defence, not the only one. It is here
            // because that first line is somebody else's implementation detail: a
            // loader change, a new source (env, a control plane), or a hand-built
            // config in a binary would all walk straight past it, and then nothing
            // else in the process would notice.
            //
            // Why it would matter, if it ever did get through:
            //
            //   * NaN reaches calculate_token_cost_idr, whose final "as i64" cast
            //     maps NaN to ZERO. A model priced nan would bill every request at
            //     0 IDR - no error, no refusal, a ledger that balances exactly, and
            //     revenue that is silently zero. Reconcile cannot see it, because
            //     the ledger faithfully records the zero it was given.
            //   * inf saturates to i64::MAX, so the reservation would exceed any
            //     wallet and every request would be refused. Loud, and still a
            //     dead service.
            //
            // Neither is a price a customer can be charged from, so the rule is
            // FINITENESS and not positivity, and it runs before the "<= 0" rules
            // so the message names the real problem.
            if !model.price.is_finite() {
                return Err(format!(
                    "Model {} has a price multiplier that is not a finite number",
                    model.name
                )
                .into());
            }
            let rates = &model.rates;
            for (field, rate) in [
                ("input_peak", rates.input_peak),
                ("output_peak", rates.output_peak),
                ("input_offpeak", rates.input_offpeak),
                ("output_offpeak", rates.output_offpeak),
                ("cache_read_peak", rates.cache_read_peak),
                ("cache_read_offpeak", rates.cache_read_offpeak),
            ] {
                if !rate.is_finite() {
                    return Err(format!(
                        "Model {} has a rate that is not a finite number: rates.{field} = {rate}",
                        model.name
                    )
                    .into());
                }
            }
            if model.price <= 0.0 {
                return Err(
                    format!("Model {} has invalid price multiplier <= 0", model.name).into(),
                );
            }
            if model.rates.input_peak <= 0.0 || model.rates.output_peak <= 0.0 {
                return Err(format!("Model {} is missing peak rates", model.name).into());
            }
            // THE CACHE-READ RATE MUST BE A REAL DISCOUNT, and nothing enforced it.
            //
            // The pre-flight hold prices the WHOLE prompt at the input rate, because
            // at reservation time there is no way to know which of the prompt tokens
            // the upstream will report as cache hits. Settlement splits those same
            // tokens in two and charges the cache subset at its own, cheaper, rate:
            //
            //     hold   = estimated_input * input_peak
            //     charge = input * input_peak + cache_read * cache_read_peak
            //
            // So the hold is a ceiling over settlement - which proxy.rs:1002 states
            // outright, without qualification - ONLY while
            // cache_read_peak <= input_peak. Flip that inequality and the ceiling
            // INVERTS: a request whose prompt is mostly cache hits settles above its
            // own hold. That is the same stranded-hold shape a cheaper endpoint
            // override produced, arriving by a different route.
            //
            // The shipped config satisfies it by a wide margin (53.54 against
            // 2676.78, a 50x discount) and nothing checked. A discount that is
            // supposed to be the norm is exactly what a later edit "corrects"
            // without noticing what it breaks, so it is a rule here rather than an
            // assumption left in a comment.
            //
            // Per class, because the two are configured independently: an offpeak
            // cache rate above the offpeak input rate is the same defect.
            //
            // AFTER the positivity rules deliberately. A zero or negative input rate
            // makes this comparison true for ANY cache rate, so running it first
            // would report "cache_read exceeds input" for a config whose actual
            // fault is a missing peak rate - sending the operator to the wrong line
            // of a six-rate block. The order is a real property and is asserted.
            for (class, input_rate, cache_rate) in [
                ("peak", rates.input_peak, rates.cache_read_peak),
                ("offpeak", rates.input_offpeak, rates.cache_read_offpeak),
            ] {
                if cache_rate > input_rate {
                    return Err(format!(
                        "Model {} {class} cache_read rate {cache_rate} exceeds its input \
                         rate {input_rate}: the pre-flight hold prices the whole prompt at \
                         the input rate, so a cache-read rate above it makes settlement \
                         exceed the hold and strand it. A cache read is a discount",
                        model.name
                    )
                    .into());
                }
                // AND IT MUST NOT BE NEGATIVE, which is NOT implied by the bound above.
                //
                // The loop above caps the cache rate at the input rate; nothing floored it.
                // A negative rate is finite and passes that test, so a config with
                // `cache_read_peak = -1e6` validated. It then propagated through
                // `money::calculate_token_cost_idr`, where the cache term is SUBTRACTED:
                // measured, input=1e3, cache=5e7, output=1e3 at r_cache=-1e6 against the
                // shipped peak rates gives a cost of -57_499_987 IDR.
                //
                // Where that lands is the reason this is a money defect and not a
                // curiosity: `db::debit_usage_transaction` passes `cost_idr` STRAIGHT to
                // `try_debit`, whose guard is `balance_idr >= ?1` - a FLOOR, not a sign
                // check. A negative amount satisfies it for any non-negative balance, and
                // `balance_idr - (-X)` CREDITS the wallet. `clamp_debit` floors the cost on
                // the OTHER path only; the settled branch does not call it. The ledger and
                // the wallet move together, so reconciliation stays clean - it is money
                // created from a configuration value.
                //
                // Negative is not a meaningful price under any reading. A zero cache rate
                // already means "cache reads are not discounted" and is accepted, so the
                // floor refuses only what cannot mean anything.
                // NaN is refused here too, and the form says so directly rather than
                // relying on a subtlety: `NaN <= 0.0` is FALSE, so the reflex `<= 0.0`
                // would let a NaN through, and the negated `!(x >= 0.0)` that fixes that
                // is what clippy's `neg_cmp_op_on_partial_ord` rightly calls hard to read.
                // Naming both cases says what is meant.
                if cache_rate.is_nan() || cache_rate < 0.0 {
                    return Err(format!(
                        "Model {} {class} cache_read rate {cache_rate} is negative or not a \
                         number. A cache read is a discount off the input rate, so it is \
                         bounded by it at the top; a negative one is not a deeper discount \
                         but a CREDIT, and it reaches try_debit as a negative cost that \
                         adds money to the wallet",
                        model.name
                    )
                    .into());
                }
            }
            // An override is what the reservation is sized from, so a 0 or
            // negative one would under-reserve silently. A MISSING override
            // falls back to the model's rate, so only a present value is checked.
            for endpoint in &model.endpoints {
                // An override is what the reservation is sized from, so a NaN here
                // under-reserves by the same route a zero does, and for the same
                // reason: the "<= 0" test below is false for it.
                // Weight is the ROUTED flag, read as "weight > 0.0" in three places
                // (client.rs:406, :462 and the endpoint-key documentation check), so it
                // suffers the same IEEE 754 blind spot from the other end: NaN > 0.0 is
                // false, so a NaN weight does not fail a check, it silently DISABLES
                // the endpoint. Nothing is ever routed to it, and because
                // all_endpoints_unhealthy only counts endpoints that passed the same
                // filter, it cannot report the model as down either. The endpoint
                // vanishes with no error and no alert.
                //
                // A non-positive weight is legitimate and means "do not route to
                // this", so only finiteness is refused here, not positivity.
                if !endpoint.weight.is_finite() {
                    return Err(format!(
                        "Model {} endpoint {} has a weight that is not a finite number: {}",
                        model.name, endpoint.name, endpoint.weight
                    )
                    .into());
                }
                for (field, rate) in [
                    ("input_peak", endpoint.input_peak),
                    ("output_peak", endpoint.output_peak),
                ] {
                    if rate.is_some_and(|rate| !rate.is_finite()) {
                        return Err(format!(
                            "Model {} endpoint {} has a peak rate override that is not a finite number: {field}",
                            model.name, endpoint.name
                        )
                        .into());
                    }
                }
                if endpoint.input_peak.is_some_and(|rate| rate <= 0.0)
                    || endpoint.output_peak.is_some_and(|rate| rate <= 0.0)
                {
                    return Err(format!(
                        "Model {} endpoint {} has a non-positive peak rate override",
                        model.name, endpoint.name
                    )
                    .into());
                }
            }
        }
        Ok(())
    }
}

/// The widest prefix accepted in `network.trusted_proxy_cidrs`, per family.
///
/// The trust list decides whose `X-Forwarded-For` is believed, and a peer that
/// is believed can choose the address it is recorded as — the exact signal
/// `docs/ip-tracking.md` exists to raise. A relay is one host or one small
/// subnet, so anything wider than a `/16` (IPv4) is not a relay rule.
///
/// IPv4 `/16`: `/8` is 16.7M addresses — the whole `10/8` private plane — and
/// `docs/topology.md:39-42` keeps the backend directly reachable for failover,
/// so "on the private network" is not a boundary. Any co-tenant or dev box in
/// that range could forge its recorded address.
///
/// IPv6 `/64`: the equivalent line for a host network. A `/16` there is 2^112
/// addresses, which is the internet.
///
/// Both a too-wide rule and a default route are fatal, not warnings: a warning
/// at boot is a log line nobody reads, and the failure it hides is a silently
/// defeated abuse signal. `0.0.0.0/0` and `::/0` are rejected outright by the
/// prefix floor, and named explicitly so the message says what was meant.
fn validate_trusted_proxy_width(cidrs: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    for text in cidrs {
        let address: std::net::IpAddr = text
            .split_once('/')
            .and_then(|(address, _)| address.trim().parse().ok())
            .ok_or_else(|| {
                format!("network.trusted_proxy_cidrs: {text}: expected ADDRESS/PREFIX")
            })?;
        let prefix: u8 = text
            .split_once('/')
            .and_then(|(_, prefix)| prefix.trim().parse().ok())
            .ok_or_else(|| {
                format!("network.trusted_proxy_cidrs: {text}: expected ADDRESS/PREFIX")
            })?;

        if prefix == 0 {
            return Err(format!(
                "network.trusted_proxy_cidrs: {text} is a default route and trusts every host \
                 on the internet; a proxy rule must name the relay"
            )
            .into());
        }

        let max_width = match address {
            std::net::IpAddr::V4(_) => 16,
            std::net::IpAddr::V6(_) => 64,
        };
        if prefix < max_width {
            return Err(format!(
                "network.trusted_proxy_cidrs: {text} is wider than /{max_width} and trusts far \
                 more hosts than the relay; any of them could set X-Forwarded-For and choose the \
                 address it is recorded as. Name the relay itself"
            )
            .into());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    /// The output side of the pre-flight hold, pinned at all four corners.
    ///
    /// The formula is max(requested, model cap).min(hard cap), and each clause
    /// exists because of a way the hold has been wrong before:
    ///
    ///   - A request that OMITS max_tokens must still hold the model's ceiling.
    ///     Holding only the client's figure left a 4096-token hold guarding a model
    ///     that emits up to 384000 tokens, and an over-long answer overdrew the
    ///     wallet.
    ///   - A request that asks for LESS than the model can produce must also hold
    ///     the model's ceiling, for the same reason.
    ///   - A request that asks for MORE than the global cap is CLAMPED to it.
    ///
    /// The last clause reads as vacuous, because hard_max_output_tokens and every
    /// model's max_output_tokens ship at the same 384000 - so this file previously
    /// carried the claim that the global cap can never bind. That claim was WRONG, and
    /// worth recording as wrong: the two fire in DIFFERENT PLACES. The model cap
    /// binds for every ordinary request, and the hard cap binds exactly when a
    /// client asks for more than it. Setting the global equal to the model cap is a
    /// BACKSTOP, not a no-op.
    #[test]
    fn the_output_hold_is_the_model_ceiling_clamped_to_the_global() {
        let config = AppConfig::load_from_file("../config/apikita.toml")
            .or_else(|_| AppConfig::load_from_file("config/apikita.toml"))
            .expect("config/apikita.toml must load");
        let model = &config.models[0];
        let hard = config.streaming.hard_max_output_tokens;

        // (1) OMITTED: no cap in the request at all.
        assert_eq!(
            model.reserved_output_tokens(None, hard),
            model.max_output_tokens,
            "a request that omits max_tokens still lets the upstream stream to the \
             model's ceiling, so the hold must be the model's own"
        );

        // (2) ASKING FOR LESS: the upstream is not obliged to stop at the client's
        // figure, so holding their cap would under-reserve.
        for asked in [1u64, 1000, 4096] {
            assert_eq!(
                model.reserved_output_tokens(Some(asked), hard),
                model.max_output_tokens,
                "a client asked for {asked} tokens but the model can emit {}, so holding \
                 their cap would under-reserve",
                model.max_output_tokens
            );
        }

        // (3) ASKING FOR MORE: this is the clamp the global cap exists for.
        for asked in [hard + 1, hard * 2, u64::MAX / 2] {
            assert_eq!(
                model.reserved_output_tokens(Some(asked), hard),
                hard,
                "a client asked for {asked} output tokens, which must clamp to the \
                 global ceiling"
            );
        }

        // (4) THE GLOBAL BELOW THE MODEL: then the hard cap binds for EVERYONE,
        // including a request that asked for nothing. Without this, reading the two
        // as equal and therefore redundant would look safe.
        let tighter = model.max_output_tokens - 1;
        assert_eq!(
            model.reserved_output_tokens(None, tighter),
            tighter,
            "when the global ceiling is below the model's own, the global binds for \
             every request - the configuration that makes the hard cap load bearing \
             rather than a backstop"
        );

        assert_eq!(
            model.max_output_tokens, hard,
            "the shipped config sets the global ceiling equal to the model's, so the \
             hard cap binds only for clients who ask for more - which it does handle"
        );
    }
    /// The provider header's claim about routability must match the WEIGHTS.
    ///
    /// The config says how many providers are verified and asserts that unverified
    /// entries are held at weight 0 so they are never routed. It once carried a
    /// placeholder at weight 1.0 - so a reader taking the header at face value would
    /// believe the router only ever offers requests to the one real supplier, when
    /// it would offer them to a URL that is not a provider. That placeholder now
    /// sits at weight 0 and the header says so.
    ///
    /// This pins the two together, because the comment is what an operator reads
    /// before deploying and the weights are what the router obeys. Raising any
    /// placeholder to a positive weight makes one of the two wrong, and the test
    /// names which.
    ///
    /// WHAT IS NOT ASSERTED, because it is a deployment decision rather than a
    /// property of the file: that the routable endpoint is the VERIFIED one. The
    /// config carries no verified flag, so nothing can check that today - which is
    /// itself why the header used to overstate what this file enforces.
    #[test]
    fn the_config_header_does_not_overstate_how_many_endpoints_are_routable() {
        let config = AppConfig::load_from_file("../config/apikita.toml")
            .or_else(|_| AppConfig::load_from_file("config/apikita.toml"))
            .expect("config/apikita.toml must load");

        // The flash list is the one the PROVIDERS AND MODELS header describes, and
        // the only one carrying more than one entry.
        let flash = config
            .models
            .iter()
            .find(|m| m.name == "flash")
            .expect("the shipped config must carry the flash model");
        let routable: Vec<&str> = flash
            .endpoints
            .iter()
            .filter(|e| e.weight > 0.0)
            .map(|e| e.name.as_str())
            .collect();

        // The header names the first as verified and warns the rest are placeholders
        // held at weight 0. Exactly one routable entry means the warning and the
        // weights agree; more than one means a placeholder has been raised without
        // the header being updated to say which.
        assert_eq!(
            routable.len(),
            1,
            "routable flash endpoints are {routable:?}, but the header describes one verified provider and says the rest are held at weight 0. If a placeholder was raised to a positive weight, the header must say so; if a real provider was added, the header must name it. Either way the two must agree."
        );
        assert!(
            routable.contains(&"primary"),
            "the verified provider must stay routable, got {routable:?}"
        );
    }

    use crate::money::calculate_preflight_reservation_idr;

    /// A config with NO per-endpoint rate overrides must load and validate
    /// EXACTLY as before: the override fields are additive and optional, so
    /// every existing config — the shipped one and every operator's — keeps
    /// working with no edit. This is the regression the feature must not cause.
    #[test]
    fn a_config_without_per_endpoint_rates_still_loads_and_prices_at_the_model_rate() {
        let toml = r#"
            [pricing]
            currency = "IDR"
            [wallet]
            min_topup = 10000
            min_first_deposit = 50000
            min_monthly_tokens = 0
            dormancy_days = 0
            reserve_settlement_cycles = 1
            low_balance_threshold_idr = 10000
            low_balance_max_per_day = 1
            [sessions]
            absolute_days = 30
            idle_days = 7
            [limits]
            topup_per_hour = 5
            wallet_mutations_per_minute = 10
            key_creation_per_day = 10
            review_per_hour = 3
            key_metadata_cache_seconds = 60
            [realtime]
            replay_buffer_events = 100
            max_connections_per_account = 5
            max_stream_seconds = 1800
            [key_pool]
            rate_limit_status = [429]
            key_cooldown_seconds = 5
            max_key_attempts = 3
            on_pool_exhausted = "reject_503"
            [circuit_breaker]
            failure_threshold = 3
            cooldown_seconds = 30
            cooldown_max_seconds = 900
            cooldown_multiplier = 2.0
            request_timeout_seconds = 120
            health_check_interval_seconds = 30
            health_check_failures = 2
            [streaming]
            mid_stream_cutoff = false
            hard_max_output_tokens = 384000
            max_context_tokens = 1000000
            [network]
            trusted_proxy_cidrs = ["127.0.0.1/32"]
            [[models]]
            name = "flash"
            description = "d"
            price = 1.5
            max_context_tokens = 1000000
            max_output_tokens = 384000
            supports_vision = true
            supports_thinking = true
            billing_basis = "peak"
            [models.rates]
            cache_read_offpeak = 26.77
            cache_read_peak = 53.54
            input_offpeak = 1338.39
            input_peak = 2676.78
            output_offpeak = 5353.56
            output_peak = 10707.12
            [[models.endpoints]]
            name = "primary"
            url = "https://a.example.com/v1"
            upstream_model = "deepseek-flash"
            [[models.endpoints]]
            name = "secondary"
            url = "https://b.example.com/v1"
            upstream_model = "deepseek-flash"
        "#;

        let config: AppConfig = toml::from_str(toml).expect("a legacy config must still parse");
        config
            .validate()
            .expect("a legacy config must still validate");

        let flash = &config.models[0];
        // Absent overrides are None, and the effective rate is the model's.
        for endpoint in &flash.endpoints {
            assert_eq!(endpoint.input_peak, None);
            assert_eq!(endpoint.output_peak, None);
            assert_eq!(endpoint.effective_input_peak(flash), flash.rates.input_peak);
            assert_eq!(
                endpoint.effective_output_peak(flash),
                flash.rates.output_peak
            );
        }

        // With no overrides the endpoints TIE, and the reservation is exactly
        // the old model-rate figure - the feature is behaviour-preserving.
        let tied = calculate_preflight_reservation_idr(1.5, 1_000_000, 2676.78, 4096, 10707.12);
        assert_eq!(flash.worst_case_reservation_idr(1_000_000, 4096), tied);
    }

    /// THE INVARIANT, and it was not being kept whenever an endpoint's rate
    /// override is LOWER than the model's.
    ///
    /// Two different rate sets are in play, and the codebase is explicit about
    /// both:
    ///
    ///   * The pre-flight HOLD is taken before routing, so it prices each endpoint
    ///     at its OWN rates (worst_case_reservation_idr, below) and keeps the
    ///     dearest. That is the documented reason per-endpoint rates exist at all:
    ///     a failover to a dearer reseller must not overdraw the balance.
    ///   * SETTLEMENT always charges the MODEL's rates. Every call site -
    ///     routes/proxy.rs:1522, :2637, :3331 - passes model_cfg.rates.* and never
    ///     the endpoint's, because by settlement time the choice is made and the
    ///     customer pays the product price, not the reseller's.
    ///
    /// So the hold is only safe if it is at least the model-rate charge. Taking the
    /// max over endpoints alone does not guarantee that: the model's own rates were
    /// only ever a fallback for an EMPTY pool, never a candidate. An endpoint that
    /// overrides DOWNWARD - which is the ordinary reason to add an override, this
    /// reseller is cheaper - therefore lowered the hold below the charge.
    ///
    /// The result is a stranded hold: the ledger records -reserved, +reserved and a
    /// -cost larger than the hold, the balance goes negative, and reconcile.sh is
    /// structurally blind to it. That is INVISIBLE MONEY, the class this repository
    /// treats as the worst kind of bug.
    ///
    /// Asserted as a RELATION rather than a figure, because the relation is the
    /// invariant and a figure would just be a snapshot of today's arithmetic.
    #[test]
    fn the_hold_covers_the_model_rate_charge_whatever_the_endpoints_override() {
        // Three shapes, cheapest-override first: the case that was broken, the case
        // the feature was built for, and the shipped no-override case.
        for (label, override_peak) in [
            ("an endpoint cheaper than the model", Some(10.0)),
            ("an endpoint dearer than the model", Some(999_999.0)),
            ("no override at all", None),
        ] {
            let mut config = AppConfig::load_from_file("../config/apikita.toml")
                .or_else(|_| AppConfig::load_from_file("config/apikita.toml"))
                .expect("config/apikita.toml must load");
            let model = &mut config.models[0];
            for endpoint in &mut model.endpoints {
                endpoint.input_peak = override_peak;
                endpoint.output_peak = override_peak;
            }

            const ESTIMATED_INPUT: u64 = 1_000_000;
            const MAX_OUTPUT: u64 = 4096;

            let hold = model.worst_case_reservation_idr(ESTIMATED_INPUT, MAX_OUTPUT);

            // What settlement will ACTUALLY charge, computed the way every call
            // site computes it: the model's rates, input and output, no cache.
            let charge = crate::money::calculate_token_cost_idr(
                model.price,
                ESTIMATED_INPUT,
                model.rates.input_peak,
                0,
                model.rates.cache_read_peak,
                MAX_OUTPUT,
                model.rates.output_peak,
            );

            assert!(
                hold >= charge,
                "{label}: the hold is {hold} IDR but settlement charges {charge} IDR at \
                 the model rate, so the request strands a hold"
            );

            // BOTH DIRECTIONS, because "hold >= charge" is a one-sided claim and
            // this fix could satisfy it in the laziest way possible - by pricing
            // every request at the model rate and dropping the dearest-endpoint
            // rule altogether. What distinguishes the two implementations is what a
            // DEARER endpoint does, and that is asserted here.
            //
            // This control was wrong when first written: it demanded the hold be
            // strictly greater for every override, which fails for the CHEAPER case
            // for the right reason - the model is the floor, so a cheaper endpoint
            // must leave the hold at the floor rather than push it below.
            match override_peak {
                // Cheaper than the model: the floor holds, the hold does not shrink.
                Some(peak) if peak < model.rates.input_peak => assert_eq!(
                    hold, charge,
                    "{label}: an override of {peak} is below the model rate {0}, so the \
                     hold must stay AT the model-rate charge rather than drop to the \
                     cheaper endpoint's",
                    model.rates.input_peak
                ),
                // Dearer: the dearest-endpoint rule must still raise the hold, and
                // this is what a fix that merely added the model as a floor would
                // silently lose.
                Some(peak) => assert!(
                    hold > charge,
                    "{label}: an override of {peak} is above the model rate {}, so the \
                     hold must be STRICTLY greater than the model-rate charge - without \
                     that the dearest-endpoint rule is gone",
                    model.rates.input_peak
                ),
                // No override: the endpoints tie with the model, so the hold is
                // exactly the model-rate charge and the feature is
                // behaviour-preserving for every config that ships today.
                None => assert_eq!(
                    hold, charge,
                    "with no override the endpoints tie with the model, so the hold is \
                     exactly the model-rate charge - the feature is behaviour-preserving"
                ),
            }
        }
    }
    #[test]
    fn test_load_apikita_toml() {
        // Test parsing the root config/apikita.toml.
        let paths = ["../config/apikita.toml", "config/apikita.toml"];
        let path = paths
            .iter()
            .find(|p| Path::new(p).exists())
            .expect("Could not find apikita.toml for testing");
        let config = AppConfig::load_from_file(path).expect("Failed to load apikita.toml");
        assert_eq!(config.pricing.currency, "IDR");
        assert_eq!(config.wallet.min_first_deposit, 50000);
        assert_eq!(config.wallet.min_topup, 10000);
        assert!(config.models.len() >= 2);
    }

    /// A config that prices against no model cannot reserve or bill, so the
    /// defect must be caught at load rather than booting a server that silently
    /// serves an empty catalogue.
    #[test]
    fn an_empty_model_list_is_refused_at_validate() {
        let mut config = AppConfig::load_from_file("../config/apikita.toml")
            .or_else(|_| AppConfig::load_from_file("config/apikita.toml"))
            .expect("config/apikita.toml must load");
        config.models.clear();
        let err = config
            .validate()
            .expect_err("an empty model list must be refused")
            .to_string();
        assert!(err.contains("At least one model"), "got {err}");
    }

    /// A non-positive price multiplier would size every reservation from zero,
    /// so it is refused at load exactly like a missing rate.
    #[test]
    fn a_zero_or_negative_model_price_is_refused_at_validate() {
        for bad in [0.0, -1.0] {
            let mut config = AppConfig::load_from_file("../config/apikita.toml")
                .or_else(|_| AppConfig::load_from_file("config/apikita.toml"))
                .expect("config/apikita.toml must load");
            config.models[0].price = bad;
            let err = config
                .validate()
                .expect_err("a non-positive price must be refused")
                .to_string();
            assert!(
                err.contains("invalid price multiplier <= 0"),
                "got {err} for price {bad}"
            );
        }
    }

    /// A NaN or infinite price is NOT caught by the "<= 0" guard, and it is not a
    /// cosmetic one: NaN <= 0.0 is FALSE in IEEE 754 - the value compares greater
    /// than nothing, including itself, so every "<= 0" test in validate() waves it
    /// straight through, and inf sails past them too.
    ///
    /// HONEST SCOPE, because the first version of this test implied more than is
    /// true. TOML accepts "nan" and "inf" as float literals and a toml::Table really
    /// does hold the non-finite value. But the toml crate then REFUSES to
    /// deserialise one into a struct field, which is the path load_from_file uses,
    /// so a config file cannot deliver a NaN price to validate() today. Measured on
    /// all three routes:
    ///
    ///     toml::from_str -> toml::Table        ACCEPTED
    ///     toml::from_str -> CircuitBreakerCfg refused
    ///     toml::Value::try_into                refused
    ///
    /// So this is defence in depth against a future loader, not a fix for a live
    /// billing outage, and the commit that added it said otherwise. The value is
    /// still worth refusing: the first line of defence is a dependency behaviour
    /// nobody here controls, and a hand-built or env-sourced config would not go
    /// through it at all.
    ///
    /// Why it would matter if it ever got through. The pricing function ends in
    /// "total_customer.ceil() as i64", and a Rust "as" cast maps NaN to ZERO, so a
    /// model priced nan would bill every request at 0 IDR - no error, no refusal,
    /// a ledger that balances perfectly, and revenue that is silently zero.
    /// Reconcile cannot see it, because the ledger faithfully records the zero.
    /// An inf is loud rather than silent: it saturates to i64::MAX, so the
    /// reservation exceeds any wallet and every request is refused.
    ///
    /// So the guard is FINITENESS, not positivity: a price that is not a number a
    /// customer can be charged from is not a price, whatever it compares against.
    #[test]
    fn a_non_finite_price_or_rate_is_refused_at_validate() {
        // The parse path is not a defence, so it is part of the claim: these are
        // real TOML float literals, not values only Rust can construct.
        for literal in ["nan", "inf", "-inf"] {
            let doc = format!("[pricing]\nprice = {literal}\n");
            let table: toml::Table = toml::from_str(&doc)
                .unwrap_or_else(|e| panic!("TOML must parse the literal {literal}: {e}"));
            let parsed = table["pricing"]["price"]
                .as_float()
                .unwrap_or_else(|| panic!("{literal} must parse as a float"));
            assert!(
                !parsed.is_finite(),
                "{literal} must parse to a non-finite value, or this test proves nothing"
            );
        }

        let non_finite = [f64::NAN, f64::INFINITY, f64::NEG_INFINITY];

        for bad in non_finite {
            let mut config = AppConfig::load_from_file("../config/apikita.toml")
                .or_else(|_| AppConfig::load_from_file("config/apikita.toml"))
                .expect("config/apikita.toml must load");
            config.models[0].price = bad;
            let err = config
                .validate()
                .expect_err("a non-finite price multiplier must be refused")
                .to_string();
            assert!(
                err.contains("not a finite number"),
                "a price of {bad} must be named as non-finite, got {err}"
            );
        }

        for bad in non_finite {
            for field in [
                "input_peak",
                "output_peak",
                "input_offpeak",
                "output_offpeak",
                "cache_read_peak",
                "cache_read_offpeak",
            ] {
                let mut config = AppConfig::load_from_file("../config/apikita.toml")
                    .or_else(|_| AppConfig::load_from_file("config/apikita.toml"))
                    .expect("config/apikita.toml must load");
                let rates = &mut config.models[0].rates;
                match field {
                    "input_peak" => rates.input_peak = bad,
                    "output_peak" => rates.output_peak = bad,
                    "input_offpeak" => rates.input_offpeak = bad,
                    "output_offpeak" => rates.output_offpeak = bad,
                    "cache_read_peak" => rates.cache_read_peak = bad,
                    "cache_read_offpeak" => rates.cache_read_offpeak = bad,
                    _ => panic!("unhandled field {field}"),
                }
                let err = config
                    .validate()
                    .expect_err("a non-finite rate must be refused")
                    .to_string();
                assert!(
                    err.contains("not a finite number"),
                    "rate {field} = {bad} must be named as non-finite, got {err}"
                );
            }
        }

        for bad in [f64::NAN, f64::INFINITY] {
            let mut config = AppConfig::load_from_file("../config/apikita.toml")
                .or_else(|_| AppConfig::load_from_file("config/apikita.toml"))
                .expect("config/apikita.toml must load");
            config.models[0].endpoints[0].input_peak = Some(bad);
            let err = config
                .validate()
                .expect_err("a non-finite endpoint override must be refused")
                .to_string();
            assert!(
                err.contains("not a finite number"),
                "an endpoint override of {bad} must be named as non-finite, got {err}"
            );
        }
    }

    /// The harm the finiteness guard exists to prevent, asserted end to end so the
    /// guard cannot be dismissed as pedantry by someone tidying the validator.
    ///
    /// This is the actual behaviour, not an inference: a Rust "as" cast maps NaN to
    /// 0, so a NaN multiplier is a FREE multiplier. If a future change to the cast,
    /// the pricing function, or the float type alters that, this fails.
    #[test]
    fn a_nan_multiplier_prices_every_request_at_zero() {
        // The literal cast, exactly as the pricing function ends.
        let cost = f64::NAN.ceil() as i64;
        assert_eq!(
            cost, 0,
            "NaN casts to ZERO, which is why a non-finite price is free usage rather than an error"
        );
        // And the real function, fed the multiplier a price = nan config supplies.
        let free = crate::money::calculate_token_cost_idr(
            f64::NAN,
            1_000_000,
            2676.78,
            0,
            53.54,
            1_000_000,
            10707.12,
        );
        assert_eq!(
            free, 0,
            "a NaN multiplier must bill 0 IDR - the invisible-money failure the finiteness guard blocks"
        );
        // The control: the same shape with a real multiplier is NOT free. Without
        // this, "the function returns 0" would pass for the wrong reason.
        let paid = crate::money::calculate_token_cost_idr(
            1.5, 1_000_000, 2676.78, 0, 53.54, 1_000_000, 10707.12,
        );
        assert!(
            paid > 0,
            "the same request at M=1.5 must cost something, or the test above proves nothing"
        );
    }
    /// The same IEEE 754 hole as the price, at the other end of the comparison.
    ///
    /// Weight is read as "weight > 0.0" wherever routing is decided, and NaN > 0.0
    /// is false. So a NaN weight does not fail a validation, it silently switches an
    /// endpoint OFF: nothing is ever routed to it. The failure is not a loud refusal
    /// but an absence, and it is invisible to the health signal as well, because
    /// all_endpoints_unhealthy counts only the endpoints that passed the same
    /// filter - a model whose every endpoint is disabled by a NaN weight reports
    /// healthy with nothing behind it.
    ///
    /// A non-positive weight is LEGAL and means "do not route here", so this asserts
    /// the discrimination the validator has to make: non-finite is refused, zero and
    /// negative are not. A guard that also rejected 0.0 would be refusing a
    /// documented feature.
    #[test]
    fn a_non_finite_endpoint_weight_is_refused_but_a_zero_one_is_legal() {
        for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let mut config = AppConfig::load_from_file("../config/apikita.toml")
                .or_else(|_| AppConfig::load_from_file("config/apikita.toml"))
                .expect("config/apikita.toml must load");
            config.models[0].endpoints[0].weight = bad;
            let err = config
                .validate()
                .expect_err("a non-finite endpoint weight must be refused")
                .to_string();
            assert!(
                err.contains("not a finite number"),
                "a weight of {bad} must be named as non-finite, got {err}"
            );
        }

        // The other half: weight 0 is how an operator parks an endpoint, and the
        // shipped config relies on the idea. Refusing it would be a regression.
        for ok in [0.0, -1.0] {
            let mut config = AppConfig::load_from_file("../config/apikita.toml")
                .or_else(|_| AppConfig::load_from_file("config/apikita.toml"))
                .expect("config/apikita.toml must load");
            config.models[0].endpoints[0].weight = ok;
            config.validate().unwrap_or_else(|e| {
                panic!("a weight of {ok} is a legal way to park an endpoint: {e}")
            });
        }
    }

    /// The sweep, closed: every f64 this config can hold is now refused when it is
    /// not finite. There were five groups - the model price, the six rate fields,
    /// the two endpoint rate overrides, the endpoint weight, and the circuit
    /// breaker multiplier - and each has a test naming what it costs to skip it.
    ///
    /// This one was already defended where it is used, so the test also pins that
    /// the two defences agree: refusing a bad multiplier at load is only correct
    /// if the fallback it replaces was reachable, and 1.0 remains legal because the
    /// breaker documents it as "disables the backoff".
    #[test]
    fn a_non_finite_circuit_breaker_multiplier_is_refused_but_one_is_legal() {
        for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let mut config = AppConfig::load_from_file("../config/apikita.toml")
                .or_else(|_| AppConfig::load_from_file("config/apikita.toml"))
                .expect("config/apikita.toml must load");
            config.circuit_breaker.cooldown_multiplier = bad;
            let err = config
                .validate()
                .expect_err("a non-finite cooldown multiplier must be refused")
                .to_string();
            assert!(
                err.contains("not a finite number"),
                "a multiplier of {bad} must be named as non-finite, got {err}"
            );
        }

        for ok in [1.0, 2.0, 0.5] {
            let mut config = AppConfig::load_from_file("../config/apikita.toml")
                .or_else(|_| AppConfig::load_from_file("config/apikita.toml"))
                .expect("config/apikita.toml must load");
            config.circuit_breaker.cooldown_multiplier = ok;
            config
                .validate()
                .unwrap_or_else(|e| panic!("a multiplier of {ok} is a documented value: {e}"));
        }
    }
    /// THE INVENTORY, so the finiteness guard cannot quietly stop covering a field.
    ///
    /// The per-field tests above each pin one group of f64s, and between them they
    /// cover every float this config holds TODAY. What they cannot do is notice the
    /// NEXT one: add a `f64` to a config struct, wire it to something that
    /// multiplies, and the suite stays green. A guard written only as a list of the
    /// things it currently catches is a guard that decays.
    ///
    /// So this walks the config for real - serialise it to TOML, descend every table
    /// and array, collect the path of every float leaf - and requires that set to
    /// match, exactly, the list of floats the finiteness rule has been taught about.
    /// Add a float anywhere and this fails naming the new path, which is the prompt
    /// to decide whether it needs a rule too. The config types derive Serialize for
    /// exactly this reason.
    ///
    /// WHY IT COMPARES INVENTORY AND NOT BEHAVIOUR. The obvious version of this test
    /// poisons each float with NaN and requires validate() to refuse it, and that is
    /// what it was written as first. It cannot work, and the reason is worth
    /// keeping: poisoning has to travel through TOML, and the toml crate REFUSES to
    /// deserialise a non-finite float into a struct field. The test failed on its
    /// first float with "invalid type: floating point NaN, expected struct
    /// CircuitBreakerConfig" - the same fact that limits the guard itself, and a
    /// reminder that a reflection test can fail for a reason that has nothing to do
    /// with what it is checking.
    ///
    /// Array indices collapse to "*", so the list describes the SHAPE of the config
    /// rather than the shipped file: adding a seventh model, or removing one, must
    /// not require editing it. The optional endpoint overrides are given values
    /// first, because a None override does not serialise and would otherwise make
    /// the shape depend on the shipped file.
    #[test]
    fn every_float_in_the_config_is_one_the_finiteness_rule_accounts_for() {
        let mut config = AppConfig::load_from_file("../config/apikita.toml")
            .or_else(|_| AppConfig::load_from_file("config/apikita.toml"))
            .expect("config/apikita.toml must load");
        // Every override on every endpoint, so the shape is the struct's rather
        // than the shipped file's.
        for model in &mut config.models {
            for endpoint in &mut model.endpoints {
                endpoint.input_peak = Some(1.0);
                endpoint.output_peak = Some(1.0);
            }
        }

        let doc = toml::Value::try_from(&config).expect("the config must serialise to TOML");
        let mut found = Vec::new();
        collect_float_paths(&doc, &mut Vec::new(), &mut found);
        let shape: std::collections::BTreeSet<String> =
            found.iter().map(|p| collapse_indices(p)).collect();

        let expected: std::collections::BTreeSet<String> = [
            "circuit_breaker.cooldown_multiplier",
            "models.*.endpoints.*.input_peak",
            "models.*.endpoints.*.output_peak",
            "models.*.endpoints.*.weight",
            "models.*.price",
            "models.*.rates.cache_read_offpeak",
            "models.*.rates.cache_read_peak",
            "models.*.rates.input_offpeak",
            "models.*.rates.input_peak",
            "models.*.rates.output_offpeak",
            "models.*.rates.output_peak",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();

        // BOTH directions, so a float that stopped existing is caught as surely as
        // one that appeared. Either way this list is where the decision is recorded:
        // does this field need a finiteness rule, and does it need a test?
        let added: Vec<_> = shape.difference(&expected).collect();
        let removed: Vec<_> = expected.difference(&shape).collect();
        assert!(
            added.is_empty() && removed.is_empty(),
            "the config holds floats the finiteness rule does not account for. NEW (decide whether each needs a rule and a test): {added:?}. GONE (a field was removed or renamed - drop it from the list): {removed:?}"
        );

        // The vacuity guard on the walk itself. If it stopped descending, the
        // comparison above would pass on an empty set and the test would be theatre.
        assert_eq!(
            shape.len(),
            expected.len(),
            "the walk and the list disagree in size, so the comparison above is not a real check"
        );
    }

    /// Replaces an array index with "*", so models.0 and models.7 share a shape.
    fn collapse_indices(path: &str) -> String {
        path.split('.')
            .map(|segment| match segment.parse::<usize>() {
                Ok(_) => "*".to_string(),
                Err(_) => segment.to_string(),
            })
            .collect::<Vec<_>>()
            .join(".")
    }

    /// Every path through a TOML document whose leaf is a float, joined with a dot.
    /// Array elements are indexed numerically, so a config with a table of floats
    /// still yields one path per element rather than a single ambiguous one.
    fn collect_float_paths(node: &toml::Value, prefix: &mut Vec<String>, out: &mut Vec<String>) {
        match node {
            toml::Value::Table(table) => {
                for (key, value) in table {
                    prefix.push(key.clone());
                    collect_float_paths(value, prefix, out);
                    prefix.pop();
                }
            }
            toml::Value::Array(items) => {
                for (index, value) in items.iter().enumerate() {
                    prefix.push(index.to_string());
                    collect_float_paths(value, prefix, out);
                    prefix.pop();
                }
            }
            toml::Value::Float(_) => out.push(prefix.join(".")),
            _ => {}
        }
    }
    /// THE CACHE-READ DISCOUNT IS A SAFETY PROPERTY, not a pricing preference.
    ///
    /// The hold prices the whole prompt at the input rate; settlement splits the
    /// same tokens and charges the cache subset at its own rate. The hold is a
    /// ceiling over settlement ONLY while cache_read_peak <= input_peak, and
    /// proxy.rs:1002 already claims it is one without qualification. This asserts
    /// the claim directly, as a relation between the two figures the validator now
    /// refuses to let diverge.
    ///
    /// The sweep matters more than any single case, because the failure is not at
    /// the extremes. A prompt that is ALL cache reads is the worst case for the
    /// hold, and an all-plain prompt passes no matter what the rules are, so a
    /// test that only tried the latter would be worthless.
    #[test]
    fn the_hold_is_a_ceiling_over_settlement_for_every_cache_split() {
        const PROMPT: u64 = 1_000_000;

        for (label, input_peak, cache_read_peak) in [
            ("the shipped 50x discount", 2676.78, 53.54),
            // Equality is the boundary the rule allows, and it must be allowed: a
            // zero-discount cache read is odd, but it strands nothing, and a guard
            // that refused it would be refusing a legitimate configuration.
            ("a zero discount", 2676.78, 2676.78),
        ] {
            let hold = crate::money::calculate_preflight_reservation_idr(
                1.5,
                PROMPT,
                input_peak,
                0,
                cache_read_peak,
            );

            for cache_read_tokens in [0, 1, PROMPT / 4, PROMPT / 2, PROMPT - 1, PROMPT] {
                let charge = crate::money::calculate_token_cost_idr(
                    1.5,
                    PROMPT - cache_read_tokens,
                    input_peak,
                    cache_read_tokens,
                    cache_read_peak,
                    0,
                    10707.12,
                );
                assert!(
                    hold >= charge,
                    "{label}: a prompt of {PROMPT} tokens with {cache_read_tokens} read \
                     from cache charges {charge} IDR against a {hold} IDR hold"
                );
            }
        }

        // And the converse, which is the point of the whole rule: past the boundary
        // the ceiling INVERTS. This is what the validator now refuses, shown rather
        // than asserted in the abstract - a guard with no demonstrated failure is a
        // guard nobody can tell apart from ceremony.
        let hold =
            crate::money::calculate_preflight_reservation_idr(1.5, PROMPT, 2676.78, 0, 53.54);
        let charge =
            crate::money::calculate_token_cost_idr(1.5, 0, 2676.78, PROMPT, 100_000.0, 0, 10707.12);
        assert!(
            charge > hold,
            "a cache rate above the input rate MUST be able to exceed the hold, or \
             refusing it in the validator would be refusing nothing"
        );
    }

    /// The rule itself, in both directions, for both rate classes, and in the right
    /// ORDER relative to the positivity checks.
    #[test]
    fn a_cache_read_rate_above_its_input_rate_is_refused_and_below_is_not() {
        for class in ["peak", "offpeak"] {
            // Above: refused, and the message must say WHICH figure is wrong.
            // "invalid config" sends an operator to the wrong line of a six-rate
            // block.
            //
            // Both figures must clear the SHIPPED input rates - 2676.78 peak,
            // 1338.39 offpeak - because the case under test is "higher than the input
            // rate". This test first used 2.0, which is far BELOW both, so every
            // assertion in the loop was vacuous and the loop proved nothing.
            for above in [5_000.0, 1_000_000.0] {
                let mut config = AppConfig::load_from_file("../config/apikita.toml")
                    .or_else(|_| AppConfig::load_from_file("config/apikita.toml"))
                    .expect("config/apikita.toml must load");
                let rates = &mut config.models[0].rates;
                if class == "peak" {
                    rates.cache_read_peak = above;
                } else {
                    rates.cache_read_offpeak = above;
                }
                let err = config
                    .validate()
                    .expect_err("a cache-read rate above the input rate must be refused")
                    .to_string();
                assert!(
                    err.contains(&format!("{class} cache_read rate")),
                    "the {class} message must name the class and the field, got {err}"
                );
                assert!(
                    err.contains("A cache read is a discount"),
                    "the message must say WHY, or the rule reads as arbitrary, got {err}"
                );
            }

            // Below and equal: accepted. Equality is the boundary the rule
            // deliberately allows, and the shipped config is far below it.
            //
            // ZERO IS IN THIS LIST ON PURPOSE. A zero cache-read rate means "cache reads
            // are not discounted", which is a coherent setting; the negative floor added
            // beside the upper bound must not swallow it.
            for ok in [1000.0, 500.0, 0.0] {
                let mut config = AppConfig::load_from_file("../config/apikita.toml")
                    .or_else(|_| AppConfig::load_from_file("config/apikita.toml"))
                    .expect("config/apikita.toml must load");
                let rates = &mut config.models[0].rates;
                if class == "peak" {
                    rates.cache_read_peak = ok;
                } else {
                    rates.cache_read_offpeak = ok;
                }
                config
                    .validate()
                    .unwrap_or_else(|e| panic!("a {class} cache rate of {ok} is legal: {e}"));
            }

            // ...and NEGATIVE is not, which the upper bound alone did not cover. A
            // negative rate is finite and below the input rate, so it passed every rule
            // here and reached `calculate_token_cost_idr`, where the cache term is
            // subtracted - a cost of -57_499_987 IDR for the worked example in the
            // validator's comment. `debit_usage_transaction` hands that straight to
            // `try_debit`, whose guard is a floor rather than a sign check, so it CREDITS
            // the wallet.
            for negative in [-1.0, -1_000_000.0] {
                let mut config = AppConfig::load_from_file("../config/apikita.toml")
                    .or_else(|_| AppConfig::load_from_file("config/apikita.toml"))
                    .expect("config/apikita.toml must load");
                let rates = &mut config.models[0].rates;
                if class == "peak" {
                    rates.cache_read_peak = negative;
                } else {
                    rates.cache_read_offpeak = negative;
                }
                let err = config
                    .validate()
                    .expect_err("a NEGATIVE cache-read rate must be refused")
                    .to_string();
                assert!(
                    err.contains("is negative or not a number"),
                    "the {class} message must say the rate is negative, got {err}"
                );
                assert!(
                    err.contains("CREDIT"),
                    "the message must say what it turns into, or the rule reads as \
                     pedantry about a discount that is merely very good, got {err}"
                );
            }
        }

        // THE ORDER IS PART OF THE RULE. A config whose peak input rate is 0 makes
        // "cache_read exceeds input" true for any cache rate, so a discount check
        // placed before the positivity check reports the wrong fault: an operator
        // reading "cache_read exceeds its input rate" would go and fix a cache
        // number that is perfectly fine, while the missing peak rate went unfixed.
        let mut config = AppConfig::load_from_file("../config/apikita.toml")
            .or_else(|_| AppConfig::load_from_file("config/apikita.toml"))
            .expect("config/apikita.toml must load");
        config.models[0].rates.input_peak = 0.0;
        let err = config
            .validate()
            .expect_err("a zero peak input rate must be refused")
            .to_string();
        assert!(
            err.contains("missing peak rates"),
            "a degenerate input rate must be reported as the degenerate rate it is, \
             not as a cache discount problem, got {err}"
        );
    }

    /// A cooldown that cannot be added to the clock is refused at load, for the
    /// three settings that actually reach an `Instant + Duration`.
    ///
    /// The boundary is measured, and it is far further out than a plausible typo
    /// would reach: a trillion seconds - 31,700 years - is fine. So this test is
    /// about naming a setting at load rather than panicking inside the breaker
    /// later, and the values below are chosen to straddle the boundary rather than
    /// to be realistic.
    ///
    /// The control matters more than the cases: a value an operator COULD write is
    /// accepted, or the guard would be refusing the configuration the repository
    /// ships.
    #[test]
    fn a_cooldown_too_large_for_the_clock_is_refused_and_a_plausible_one_is_not() {
        for (field, absurd, plausible) in [
            ("cooldown_seconds", u64::MAX, 30u64),
            ("cooldown_max_seconds", u64::MAX, 900),
            ("key_cooldown_seconds", u64::MAX, 5),
        ] {
            let mut config = AppConfig::load_from_file("../config/apikita.toml")
                .or_else(|_| AppConfig::load_from_file("config/apikita.toml"))
                .expect("config/apikita.toml must load");

            set_duration(&mut config, field, absurd);
            let err = config
                .validate()
                .expect_err("a cooldown the clock cannot represent must be refused")
                .to_string();
            assert!(
                err.contains(field) && err.contains("cannot be a cooldown"),
                "the message must name the setting and the problem, got {err}"
            );

            // The control: an ordinary value still loads. Without this, a guard that
            // refused everything would pass.
            set_duration(&mut config, field, plausible);
            config
                .validate()
                .unwrap_or_else(|e| panic!("{field} = {plausible} is a normal cooldown: {e}"));
        }

        // And the SHIPPED config, which is the one that has to keep working.
        AppConfig::load_from_file("../config/apikita.toml")
            .or_else(|_| AppConfig::load_from_file("config/apikita.toml"))
            .expect("the shipped config must still load and validate");
    }

    /// Sets one of the three cooldown settings by name. A helper rather than three
    /// copies of a match, so adding a fourth field to the rule does not mean
    /// editing a fourth arm here.
    fn set_duration(config: &mut AppConfig, field: &str, secs: u64) {
        match field {
            "cooldown_seconds" => config.circuit_breaker.cooldown_seconds = secs,
            "cooldown_max_seconds" => config.circuit_breaker.cooldown_max_seconds = secs,
            "key_cooldown_seconds" => config.key_pool.key_cooldown_seconds = secs,
            _ => panic!("unhandled field {field}"),
        }
    }

    /// A session lifetime that cannot be added to the clock is refused at load.
    ///
    /// The boundary is measured, and it is much CLOSER than the cooldown one: a
    /// session lifetime is in DAYS, so nine digits is enough, where a cooldown in
    /// seconds needed nineteen. The values below straddle the boundary rather than
    /// being realistic, and the control matters more than the cases - a guard that
    /// refused every value would pass the first half.
    ///
    /// `idle_days` is the one worth singling out. The absolute bound is evaluated
    /// once, when a session is created; the idle bound is added on EVERY session
    /// resolution, so an unrepresentable value would panic on the first request and
    /// every request after it.
    #[test]
    fn a_session_lifetime_too_large_for_the_clock_is_refused_and_a_real_one_is_not() {
        for field in ["absolute_days", "idle_days"] {
            let mut config = AppConfig::load_from_file("../config/apikita.toml")
                .or_else(|_| AppConfig::load_from_file("config/apikita.toml"))
                .expect("config/apikita.toml must load");

            set_session_days(&mut config, field, u32::MAX);
            let err = config
                .validate()
                .expect_err("a session lifetime the clock cannot represent must be refused")
                .to_string();
            assert!(
                err.contains(field) && err.contains("cannot be a session lifetime"),
                "the message must name the setting and the problem, got {err}"
            );

            for real in [1u32, 7, 30, 365] {
                set_session_days(&mut config, field, real);
                config
                    .validate()
                    .unwrap_or_else(|e| panic!("{field} = {real} is a real lifetime: {e}"));
            }
        }

        // And the shipped config, which is the one that has to keep working.
        AppConfig::load_from_file("../config/apikita.toml")
            .or_else(|_| AppConfig::load_from_file("config/apikita.toml"))
            .expect("the shipped config must still load and validate");
    }

    fn set_session_days(config: &mut AppConfig, field: &str, days: u32) {
        match field {
            "absolute_days" => config.sessions.absolute_days = days,
            "idle_days" => config.sessions.idle_days = days,
            _ => panic!("unhandled field {field}"),
        }
    }

    /// EVERY CONFIG FIELD MUST BE READ BY PRODUCTION CODE, or be on a list below
    /// with a reason.
    ///
    /// WHY THIS EXISTS. The link-code issuance cap was a guard that parsed, ran, and
    /// could never fire - it counted a table its own handler DELETEs from. Nothing
    /// about it looked wrong, and the suite was green. Probing for that shape by
    /// hand found fourteen more fields in the same state: documented settings that
    /// nothing reads, including two that docs/decisions.md presents as a capability
    /// the breaker module explicitly disclaims ("no I/O" - recovery is a trial
    /// request on real traffic, not a background job).
    ///
    /// A hand-run probe finds today's dead fields and none of tomorrow's. This is
    /// the probe, as a test, so the next one is a red build rather than an
    /// afternoon's reading.
    ///
    /// HOW THE INVENTORY IS BUILT. Not by parsing struct definitions with a regex -
    /// by serialising the loaded config to TOML and walking the result. That cannot
    /// drift from the real schema, because it IS the real schema: a field added and
    /// forgotten appears here without anybody editing a list.
    ///
    /// AND THE LIMITATION, stated because a test that quietly under-reports is worse
    /// than none. The search requires a FIELD ACCESS - the name preceded by a dot - so
    /// a struct declaration, a TOML literal and the parser key list stop counting as
    /// uses. That is what found the parsed-but-unread keys, and it is why the exact
    /// floor `leaves.len() > 40` below is pinned rather than left as a comment.
    ///
    /// Two things it still cannot see, both measured rather than assumed:
    ///
    /// A field read through a macro or a generated accessor has no dot in the source.
    /// Nothing in this crate reads config that way, so this is a limit rather than a
    /// live gap - but it is the limit a name search has, and it is the one to check
    /// first if a field is reported dead and looks alive.
    ///
    /// And more importantly: a field read ONLY BY DEAD CODE is indistinguishable from
    /// a wired one. That was measured too. `max_context_tokens` was reported as wired
    /// because one production function read it - a second `worst_case_reservation_idr`
    /// on `UpstreamClient` that no request ever called, so the read existed and never
    /// ran. Deleting that function is what moved the field onto the register below.
    /// No source-level check can close this one; a call graph could, which is what
    /// docs/testing.md points at.
    #[test]
    fn every_config_field_is_read_by_production_code_or_explained() {
        /// DELIBERATELY NOT WIRED, each with the reason it is still here.
        ///
        /// An entry earns its place by being either an honest leftover from a
        /// blueprint or a decision not to build something yet. "Unknown" is not an
        /// acceptable reason and is not used.
        const UNWIRED: &[(&str, &str)] = &[
            (
                "health_check_interval_seconds",
                "there is no active health check. The breaker is deliberately PASSIVE -\
                 its module doc says 'no I/O' and recovery is a trial request on real\n                 traffic. This knob and health_check_failures are leftovers from the\n                 blueprint, and docs/decisions.md still lists 'health checks' under\n                 [circuit_breaker], which is the part that is wrong.",
            ),
            (
                "health_check_failures",
                "Same as health_check_interval_seconds: no background prober exists to\n                 count its failures.",
            ),
            (
                "wallet_mutations_per_minute",
                "no wallet-mutation rate limit is implemented. The wallet is only\n                 mutated by settlement, webhook and topup paths, each with its own\n                 bound; there is no per-minute limiter in front of them.",
            ),
            (
                "dormancy_days",
                "no dormancy sweep reads it. purge_expired covers usage, sessions and\n                 the link-code counters; nothing ages a wallet out.",
            ),
            (
                "low_balance_threshold_idr",
                "no low-balance warning is emitted. The threshold is configured and\n                 never compared against anything.",
            ),
            (
                "low_balance_max_per_day",
                "Same as low_balance_threshold_idr - the cap on a warning that is never\n                 sent.",
            ),
            (
                "mid_stream_cutoff",
                "the mid-stream cut-off is always OFF and the flag is not consulted.\n                 The behaviour it names does not exist in either state.",
            ),
            (
                "on_pool_exhausted",
                "only reject_503 is implemented, and the config comment says the\n                 other option 'needs a queue'. A one-valued enum is not a setting.",
            ),
            (
                "supports_vision",
                "advertised on the model but never used to gate or annotate a request.\n                 No capability is enforced or reported from it.",
            ),
            (
                "supports_thinking",
                "Same as supports_vision - declared, never read for a decision.",
            ),
            (
                "billing_basis",
                "every price is computed from the PEAK rates; the offpeak class is\n                 configured on the model and never selected. Selecting it would be a\n                 pricing change, which is why this is listed rather than fixed.",
            ),
            (
                "concurrency_per_key",
                "the key pool applies one global concurrency policy; the per-endpoint\n                 override is parsed and never consulted.",
            ),
            (
                "min_monthly_tokens",
                "no monthly token floor is applied to anything. The only place this \
                 name appeared outside the struct was a SENTENCE in abuse.rs citing it \
                 as an example of the zero-disables-the-cap convention - and a comment \
                 is not a use. Found by the guard once it stopped counting comments as \
                 reads, which is the same failure as a dead guard: a citation in prose \
                 protected a field nothing enforces.",
            ),
            (
                "reserve_settlement_cycles",
                "the sharpest form of inert. The reservation IS sized to cover exactly \
                 one settlement cycle - worst_case_reservation_idr does it by \
                 construction - so this setting DESCRIBES a hardcoded behaviour while \
                 reading as a control. Setting it to 5 would change nothing, and the doc \
                 comment in bin/hold-sweep.rs presents it as if it would. A value that \
                 restates what the code already does is a documentation line wearing a \
                 config field's clothes.",
            ),
            (
                "max_context_tokens",
                "NOT A CAP, despite what the register used to call it - nothing refuses \
                 a request whose prompt exceeds it. The hold is sized from the ACTUAL \
                 body (estimated_input_tokens), so an over-long prompt is still \
                 covered; the context ceiling simply bounds nothing.\n\n                 It had exactly one production reader, and that reader was DEAD: a second \
                 worst_case_reservation_idr on UpstreamClient which proxied to a \
                 ModelEntry copy and was called by nothing outside its own tests. So \
                 the field looked wired - the guard could see a read - while the read \
                 itself never ran. That is a SECOND-ORDER blind spot in the wiring \
                 guard, and the only reason this is found is that the dead function \
                 was deleted; a field read solely by unreachable code is \
                 indistinguishable from a live one by any source-level check.",
            ),
            (
                "description",
                "the model's human description is carried but never returned to a\n                 client. Harmless, and the obvious use is a future model-listing\n                 response.",
            ),
            // FOUND BY THIS TEST, NOT BY THE PROBE THAT PROMPTED IT. The hand-run
            // probe that motivated this test listed fourteen fields and missed these
            // four, which is the argument for having the check at all: a probe run
            // once finds what its author remembered to look for.
            (
                "input_offpeak",
                "the offpeak class is configured and never charged. Settlement prices\n                 every token from the PEAK rates, and the field that would select the\n                 class - billing_basis - is itself unwired. The name appears in\n                 validate() twice: read to reject a non-finite rate, and read again for\n                 the peak/offpeak cache-discount comparison, so a bad offpeak rate is\n                 caught at startup. NEITHER READ PRICES ANYTHING. That is exactly the\n                 shape the wiring check cannot see - both are reads with a dot in\n                 front, so this entry fails the stale assertion rather than passing\n                 it, and it is kept because deleting it would delete the disclosure. A\n                 rate that is validated but never charged is a rate an operator\n                 believes is in effect.",
            ),
            (
                "output_offpeak",
                "Same as input_offpeak: validated, never charged. Read only by the\n                 finiteness loop, which is why the stale check flags it too.",
            ),
            (
                "cache_read_offpeak",
                "Same as input_offpeak. Read by the finiteness loop and by the\n                 peak/offpeak cache-discount comparison, which is a real check and not\n                 a price. The discount validate() enforces is between the cache rate\n                 and the input rate of the SAME class, so checking the offpeak pair is\n                 meaningful even though charging it is not implemented.",
            ),
        ];

        let config = AppConfig::load_from_file("../config/apikita.toml")
            .or_else(|_| AppConfig::load_from_file("config/apikita.toml"))
            .expect("config/apikita.toml must load");
        let doc = toml::Value::try_from(&config).expect("the config must serialise to TOML");

        let mut leaves = std::collections::BTreeSet::new();
        collect_leaves(&doc, &mut leaves);
        assert!(
            leaves.len() > 40,
            "the inventory found only {} leaves, so the walk is broken and the check\n             below would pass vacuously",
            leaves.len()
        );

        // The production source of EVERY module, this one included, with each
        // cfg(test) block removed. Without the strip the answer is a uniform false
        // negative: the test fixtures build a whole AppConfig literal and every
        // field therefore appears read.
        //
        // THIS FILE IS NO LONGER SKIPPED, and that was a measured fix rather than a
        // tidy-up. It used to be excluded, on the reasoning that skipping the module
        // where every field is DECLARED removes the most obvious source of false
        // "wired" answers. It does - and it also removes every field access that
        // happens to live here, which is where the config types' own methods are.
        // `max_output_tokens` is read in production, by
        // `reserved_output_tokens` (`self.max_output_tokens`), and the guard could
        // not see it because that method is in this file. The first run of the
        // field-access rule below reported the field as read by nothing. Since the
        // test module is stripped before the corpus is used, skipping this file was
        // never what made the fixtures harmless - the strip was. The cost of
        // including it is that a field named ONLY in a production doc comment in
        // this file now counts as read, which is the under-reporting direction the
        // note above the helper already warns about.
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut corpus = String::new();
        collect_rust_source(&root, "", &mut corpus);
        assert!(
            !corpus.is_empty(),
            "no production source was read, so every field would look unwired"
        );

        let mut unwired: Vec<&str> = UNWIRED.iter().map(|(field, _)| *field).collect();
        unwired.sort_unstable();

        let mut missing: Vec<String> = Vec::new();
        for leaf in &leaves {
            if source_reads_field(&corpus, leaf) {
                continue;
            }
            if unwired.binary_search(&leaf.as_str()).is_ok() {
                continue;
            }
            missing.push(leaf.clone());
        }
        missing.sort();
        assert!(
            missing.is_empty(),
            "these config fields are read by NOTHING in production code and are not\n             on the UNWIRED list with a reason: {missing:?}. Either wire them up or\n             explain why they are still here - a setting nobody reads is a setting an\n             operator believes is working."
        );

        // THE REGISTER MUST NAME EVERY ONE OF THEM. docs/decisions.md is the single
        // source for what this system does, and its own rule is that a stale decision
        // is worse than none "because it is followed" - yet four of its config rows
        // claimed capabilities the code does not have, an active health check among
        // them. So the disclosure belongs in the REGISTER, and not only here, where
        // the next person to read a config table would not see it.
        //
        // Requiring the NAME rather than a judgement about the prose is what keeps
        // this checkable. The register is prose, and a rule that tried to parse it
        // would be brittle in a way that would itself rot. What can be asserted
        // cheaply and durably is this: a field that is configured, unwired, and
        // unnamed in the register is precisely the case that matters, because an
        // operator has no way to learn it is inert.
        let register = std::fs::read_to_string("../docs/decisions.md")
            .or_else(|_| std::fs::read_to_string("docs/decisions.md"))
            .expect("docs/decisions.md must be readable");
        let undisclosed: Vec<&str> = UNWIRED
            .iter()
            .map(|(field, _)| *field)
            .filter(|field| !register.contains(field))
            .collect();
        assert!(
            undisclosed.is_empty(),
            "these config fields are unwired and are NOT named in docs/decisions.md: \
             {undisclosed:?}. A setting nobody reads that the register also does not \
             mention is a setting an operator has no way to learn is inert. Name it \
             under Not enforced in the config table, or wire it up."
        );

        // And the other direction, which is what stops the list rotting into a
        // graveyard: a field that HAS been wired must come off it, or the list goes on
        // claiming things that are no longer true.
        //
        // "READ" HERE MEANS READ FOR SOMETHING, not merely fetched. Requiring only a
        // dot in front flagged three fields that are genuinely unwired and are
        // deliberately listed: the offpeak rate class. `validate()` reads all three -
        // once in the finiteness loop and once in the peak/offpeak discount comparison
        // - and none of those reads prices a token. Settlement prices from the PEAK
        // rates only. So the dot rule was too blunt in this one direction: it cannot
        // tell a read that decides behaviour from a read that inspects a value and
        // moves on, and every startup VALIDATION takes that second shape.
        //
        // Those three are therefore EXEMPT rather than deleted, and the exemption is a
        // short explicit list instead of a widening of the rule, because widening the
        // rule is exactly how a check stops being able to fail. The cost is stated
        // plainly: a field on this list can be read all over `validate()` forever, and
        // nothing will object. What the list has to stay honest about is narrower than
        // it used to be - these fields do not affect a CHARGE - and the UNWIRED entry
        // for each says so in those words.
        const READ_ONLY_FOR_VALIDATION: &[&str] =
            &["input_offpeak", "output_offpeak", "cache_read_offpeak"];
        let stale: Vec<&str> = UNWIRED
            .iter()
            .map(|(field, _)| *field)
            .filter(|field| !READ_ONLY_FOR_VALIDATION.contains(field))
            .filter(|field| source_reads_field(&corpus, field))
            .collect();
        assert!(
            stale.is_empty(),
            "these fields are on the UNWIRED list but are now read by production\n             code, so the list is claiming something untrue: {stale:?}"
        );
    }

    /// Does the production corpus READ this field, or merely NAME it?
    ///
    /// A bare `contains` answered the wrong question, and the difference was measured
    /// twice rather than theorised once.
    ///
    /// The first correction was the `.` at the front. Without it, a name that appears
    /// in prose counts as a use: the register below cites `min_monthly_tokens` in a
    /// sentence, and a sentence is not a use. Comments are stripped before this runs,
    /// which removed the whole-line case, but a doc comment is not the only place a
    /// name appears without being read - a string literal is a name, an `assert!`
    /// message is a name, and several of these names collide with ordinary English
    /// (`name`, `description`, `weight`).
    ///
    /// The second correction was to stop skipping `config.rs` when the corpus is
    /// built, and it was found by this rule rather than by looking for it: with the
    /// dot required and this file still excluded, `max_output_tokens` was reported as
    /// read by nothing, and it is read - by `reserved_output_tokens`, whose body is
    /// `self.max_output_tokens`. The config types' own methods live in the file the
    /// corpus skipped, so the guard could not see the single most natural way one of
    /// these fields gets used. The strip of the test module is what keeps the fixtures
    /// from answering for everything; excluding this file was never doing that work.
    ///
    /// Requiring a preceding dot is the cheap approximation of a field access, and it
    /// errs in the direction that reports MORE fields as unwired, because the failure
    /// this guard exists to prevent is a setting an operator believes is working. A
    /// field read as `self.rate` still matches: only the dot has to precede it.
    fn source_reads_field(corpus: &str, field: &str) -> bool {
        let dotted = format!(".{field}");
        corpus.contains(&dotted)
    }

    /// Every scalar leaf key in a TOML document, deduplicated by NAME rather than by
    /// path - rates.input_peak and an endpoint's input_peak are the same question, and
    /// the answer is the same for both.
    fn collect_leaves(node: &toml::Value, out: &mut std::collections::BTreeSet<String>) {
        match node {
            toml::Value::Table(table) => {
                for (key, value) in table {
                    match value {
                        toml::Value::Table(_) | toml::Value::Array(_) => collect_leaves(value, out),
                        _ => {
                            out.insert(key.clone());
                        }
                    }
                }
            }
            toml::Value::Array(items) => {
                for item in items {
                    collect_leaves(item, out);
                }
            }
            _ => {}
        }
    }

    /// Every .rs file under dir except skip, with test modules stripped, concatenated.
    /// Walks the tree the crate actually ships rather than a list kept by hand, so a
    /// new module is covered without being remembered.
    fn collect_rust_source(dir: &std::path::Path, skip: &str, out: &mut String) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                collect_rust_source(&path, skip, out);
            } else if path.extension().is_some_and(|e| e == "rs")
                && path.file_name().is_some_and(|n| n != skip)
            {
                let Ok(text) = std::fs::read_to_string(&path) else {
                    continue;
                };
                // COMMENTS ARE STRIPPED, and that came from a real miss rather than
                // from foresight. abuse.rs cites min_monthly_tokens in a sentence
                // explaining the "zero disables the cap" convention, and the guard read
                // that sentence as a use - so a field that nothing enforces was
                // reported as wired. A comment is not a use, and a guard that cannot
                // tell the difference will protect dead fields indefinitely.
                //
                // Only WHOLE-LINE comments are removed, plus block comments. A naive
                // scan for "//" would truncate every line containing a URL - the
                // codebase has several - and could drop a real read that happened to
                // share a line with one, which is the direction this guard must never
                // fail in. A trailing comment after code on the same line is the
                // residual gap, and it is narrow enough to state rather than solve.
                let mut kept = String::with_capacity(text.len());
                let mut in_block_comment = false;
                for line in text.lines() {
                    let trimmed = line.trim_start();
                    if in_block_comment {
                        if let Some((_, after)) = trimmed.split_once("*/") {
                            in_block_comment = false;
                            kept.push_str(after);
                            kept.push('\n');
                        }
                        continue;
                    }
                    if let Some(rest) = trimmed.strip_prefix("/*") {
                        match rest.split_once("*/") {
                            Some((_, after)) => kept.push_str(after),
                            None => in_block_comment = true,
                        }
                        kept.push('\n');
                        continue;
                    }
                    if trimmed.starts_with("//") {
                        continue;
                    }
                    kept.push_str(line);
                    kept.push('\n');
                }
                let text = kept;

                // Truncate at the LAST "mod tests", NOT at the first
                // "#[cfg(test)]". Several files carry a test-only attribute on an
                // individual item - proxy.rs has two, at lines 49 and 612 - and
                // truncating at the first one discarded fifteen hundred lines of
                // PRODUCTION code, including the settlement that charges
                // cache_read_peak. The guard then reported a live field as dead,
                // which is the one direction a completeness check must not fail in:
                // it would have sent someone to wire up something already wired.
                //
                // "mod tests" is the convention and it lives at the end, so the LAST
                // occurrence is the right anchor. A file with a mid-file test module
                // would be over-stripped here, which is the safe direction: it can
                // only report a field as unwired, never as wired.
                match text.rfind("mod tests") {
                    Some(at) => out.push_str(&text[..at]),
                    None => out.push_str(&text),
                }
                out.push('\n');
            }
        }
    }
    /// A non-positive override would size the hold from a rate that reserves
    /// nothing, so it is refused at load like a non-positive model rate.
    #[test]
    fn a_non_positive_per_endpoint_rate_is_refused_at_load() {
        let mut config = AppConfig::load_from_file("../config/apikita.toml")
            .or_else(|_| AppConfig::load_from_file("config/apikita.toml"))
            .expect("config/apikita.toml must load");
        config.models[0].endpoints[0].input_peak = Some(0.0);
        let err = config
            .validate()
            .expect_err("a zero per-endpoint rate must be refused")
            .to_string();
        assert!(err.contains("non-positive peak rate override"), "got {err}");
    }

    /// A model whose peak rates are absent reserves nothing and can never bill
    /// the peak it is documented to charge, so the missing figure is fatal.
    #[test]
    fn a_model_without_peak_rates_is_refused_at_validate() {
        for field in ["input_peak", "output_peak"] {
            let mut config = AppConfig::load_from_file("../config/apikita.toml")
                .or_else(|_| AppConfig::load_from_file("config/apikita.toml"))
                .expect("config/apikita.toml must load");
            if field == "input_peak" {
                config.models[0].rates.input_peak = 0.0;
            } else {
                config.models[0].rates.output_peak = 0.0;
            }
            let err = config
                .validate()
                .expect_err("a model without peak rates must be refused")
                .to_string();
            assert!(err.contains("missing peak rates"), "got {err} for {field}");
        }
    }

    /// A trust entry with no `/PREFIX` is not an ADDRESS/PREFIX pair, and a
    /// silent skip would trust the wrong relay — so the malformed rule is named
    /// and rejected, not forgiven. Exercised directly because `parse_cidrs`
    /// (run by `validate`) rejects a bare token with its own message and never
    /// reaches this guard.
    #[test]
    fn a_trust_entry_without_a_prefix_is_rejected() {
        let err = validate_trusted_proxy_width(&["not-an-address".to_string()])
            .expect_err("a trust entry without a prefix must be rejected")
            .to_string();
        assert!(err.contains("expected ADDRESS/PREFIX"), "got {err}");
    }

    /// A trust entry whose prefix will not parse (`/abc`) reaches the second
    /// parse guard in `validate_trusted_proxy_width` — the address parses but
    /// the prefix does not, which must also be named and rejected. This is
    /// exercised directly because `parse_cidrs` (run by `validate`) rejects a
    /// malformed prefix with its own message and never reaches this guard.
    #[test]
    fn a_trust_entry_with_an_unparseable_prefix_is_rejected() {
        let err = validate_trusted_proxy_width(&["10.0.0.1/abc".to_string()])
            .expect_err("an unparseable prefix must be rejected")
            .to_string();
        assert!(err.contains("expected ADDRESS/PREFIX"), "got {err}");
    }

    /// The shipped default must survive the width rule it is documented to
    /// obey — otherwise the local stack stops booting.
    #[test]
    fn the_shipped_trust_list_passes_width_validation() {
        let config = AppConfig::load_from_file("../config/apikita.toml")
            .or_else(|_| AppConfig::load_from_file("config/apikita.toml"))
            .expect("config/apikita.toml must load and validate");
        assert_eq!(
            config.network.trusted_proxy_cidrs,
            vec!["127.0.0.1/32", "::1/128", "172.21.0.0/16"]
        );
    }

    /// Trusting everyone is never correct: a /0 entry means any host can set
    /// X-Forwarded-For and choose the address it is recorded as.
    #[test]
    fn a_default_route_is_rejected_and_named() {
        for entry in ["0.0.0.0/0", "::/0"] {
            let err = validate_trusted_proxy_width(&[entry.to_string()])
                .expect_err("a default route must be rejected");
            let message = err.to_string();
            assert!(
                message.contains(entry),
                "message must name {entry}: {message}"
            );
            assert!(
                message.contains("default route"),
                "message must say what is wrong: {message}"
            );
        }
    }

    /// A /8 is the whole private plane. That is the confirmed defect: any
    /// co-tenant workload could forge its recorded address. /16 is the floor —
    /// it is the size of a Docker bridge, and it is accepted below.
    #[test]
    fn a_prefix_wider_than_slash_16_is_rejected_and_named() {
        for entry in ["10.0.0.0/8", "172.16.0.0/12", "192.168.0.0/15", "fe80::/10"] {
            let err = validate_trusted_proxy_width(&[entry.to_string()])
                .expect_err("a prefix wider than /16 must be rejected");
            assert!(
                err.to_string().contains(entry),
                "message must name the offending entry: {err}"
            );
        }
        // The exact shipped default, before the fix, is the regression case.
        assert!(validate_trusted_proxy_width(&[
            "127.0.0.1/32".to_string(),
            "::1/128".to_string(),
            "172.16.0.0/12".to_string(),
            "10.0.0.0/8".to_string(),
            "192.168.0.0/16".to_string(),
        ])
        .is_err());
    }

    /// A single relay address is the production shape and must be accepted,
    /// including a /16 and a /64 boundary rule.
    #[test]
    fn a_relay_sized_rule_is_accepted() {
        for entry in [
            "203.0.113.7/32",
            "172.21.0.0/16",
            "::1/128",
            "2001:db8::/64",
        ] {
            assert!(
                validate_trusted_proxy_width(&[entry.to_string()]).is_ok(),
                "{entry} names a relay-sized network and must be accepted"
            );
        }
    }

    /// An empty list is legal — it disables the header entirely, which is the
    /// safe direction — and a malformed entry is still named by `parse_cidrs`.
    #[test]
    fn an_empty_list_is_allowed() {
        assert!(validate_trusted_proxy_width(&[]).is_ok());
    }
    /// Every API-key variable the config names must be DOCUMENTED, unless its endpoint
    /// can never be routed.
    ///
    /// `.env.example` states the contract: "Names must match the `api_key_envs` arrays in
    /// config/apikita.toml." Nothing enforced it. Measured when this test was added: the
    /// config names 12 key variables and the file documents 8. The four omissions are all
    /// CORRECT - three are the `APK_DUMMY_*` names on the placeholder models and the
    /// fourth is on an endpoint carrying `weight = 0.0` - so the test allows exactly that
    /// exemption and requires everything else to be documented.
    ///
    /// WHY IT MATTERS EVEN THOUGH THE CURRENT FILE IS RIGHT. An operator copies
    /// `.env.example` and nothing tells them a key is missing: `keys_from_env`
    /// (upstream/client.rs:608) filters an unset variable out without a word, and the
    /// startup line reports `models_count`, which counts MODELS, not usable KEYS. A real
    /// endpoint whose variable nobody documented would therefore fail SILENTLY - the
    /// model simply never routes - leaving the operator no signal to work from.
    ///
    /// The endpoint weight is what separates the two cases, so the exemption is keyed on
    /// it rather than on a list of names: a name list would rot the moment someone added
    /// a placeholder, and weight is the SAME condition the router filters on
    /// (upstream/client.rs:462).
    #[test]
    fn every_routable_endpoint_key_is_documented_in_the_env_example() {
        let config = AppConfig::load_from_file("../config/apikita.toml")
            .or_else(|_| AppConfig::load_from_file("config/apikita.toml"))
            .expect("the shipped config must parse");
        let env = fs::read_to_string("../.env.example")
            .or_else(|_| fs::read_to_string(".env.example"))
            .expect("the committed .env.example must be readable");

        // Guard the FIXTURE before the assertions: a config with no endpoints, or an
        // .env.example the reader got nothing from, would make both sides empty and the
        // comparison pass vacuously.
        let mut named = 0usize;
        let mut routable_named = 0usize;
        for model in &config.models {
            for endpoint in &model.endpoints {
                named += endpoint.api_key_envs.len();
                if endpoint.weight > 0.0 {
                    routable_named += endpoint.api_key_envs.len();
                }
            }
        }
        assert!(
            named >= 10,
            "the config named only {named} key variables - the fixture read nothing"
        );
        assert!(
            routable_named >= 5,
            "only {routable_named} key variables belong to a ROUTED endpoint, so this test \
             could not tell an exemption from a gap"
        );
        assert!(
            env.matches('=').count() >= 10,
            "the .env.example fixture looks empty"
        );

        let documented: Vec<&str> = env
            .lines()
            .filter_map(|line| line.split_once('=').map(|(name, _)| name.trim()))
            .collect();

        let mut missing = Vec::new();
        let mut exempt = Vec::new();
        for model in &config.models {
            for endpoint in &model.endpoints {
                for key in &endpoint.api_key_envs {
                    if documented.contains(&key.as_str()) {
                        continue;
                    }
                    if endpoint.weight <= 0.0 {
                        exempt.push(format!("{key} (endpoint {} is weight 0)", endpoint.name));
                    } else {
                        missing.push(format!("{key} (endpoint {})", endpoint.name));
                    }
                }
            }
        }

        assert!(
            missing.is_empty(),
            ".env.example does not document {missing:#?}, and those keys belong to ROUTED \
             endpoints - an operator copying the file would get a model that silently never \
             routes, because an unset variable is filtered out without a warning. Document \
             them, or set the endpoint weight to 0 if it is truly unroutable. Exemptions \
             already taken: {exempt:#?}"
        );

        // The exemption must be REAL, not a loophole: if every endpoint were weight 0 the
        // assertion above would pass on a config with nothing routable in it.
        assert!(
            !exempt.is_empty() && exempt.len() < named,
            "the exemption is degenerate, so this test has no power: {exempt:#?}"
        );
    }
}
