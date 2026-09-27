use serde::Deserialize;
use std::fs;
use std::path::Path;

#[derive(Debug, Clone, Deserialize)]
pub struct AppConfig {
    pub pricing: PricingConfig,
    pub wallet: WalletConfig,
    pub sessions: SessionsConfig,
    pub limits: LimitsConfig,
    pub realtime: RealtimeConfig,
    pub key_pool: KeyPoolConfig,
    pub circuit_breaker: CircuitBreakerConfig,
    pub streaming: StreamingConfig,
    pub network: NetworkConfig,
    pub models: Vec<ModelConfig>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct NetworkConfig {
    /// CIDRs of reverse proxies allowed to speak for the caller.
    ///
    /// Only a peer inside one of these networks has its `X-Forwarded-For`
    /// consulted; from anywhere else the header is ignored and the TCP peer is
    /// recorded. `docs/ip-tracking.md` — the header is client-controlled, and
    /// trusting it blindly lets a caller pin its own recorded address.
    pub trusted_proxy_cidrs: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PricingConfig {
    pub currency: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct WalletConfig {
    pub min_topup: u64,
    pub min_first_deposit: u64,
    pub min_monthly_tokens: u64,
    pub dormancy_days: u32,
    pub reserve_settlement_cycles: u32,
    pub low_balance_threshold_idr: u64,
    pub low_balance_max_per_day: u32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SessionsConfig {
    pub absolute_days: u32,
    pub idle_days: u32,
}

#[derive(Debug, Clone, Deserialize)]
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
    pub key_metadata_cache_seconds: u64,
}

/// Default for `LimitsConfig::link_code_issuance_per_hour` when a config file
/// predates the key. Ten codes an hour is far more than a human needs and far
/// less than a code farm wants.
fn default_link_code_issuance_per_hour() -> u32 {
    10
}

/// Default for `LimitsConfig::link_redemption_per_hour` when a config file
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

#[derive(Debug, Clone, Deserialize)]
pub struct RealtimeConfig {
    pub replay_buffer_events: usize,
    pub max_connections_per_account: usize,
    pub max_stream_seconds: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct KeyPoolConfig {
    pub rate_limit_status: Vec<u16>,
    pub key_cooldown_seconds: u64,
    pub max_key_attempts: usize,
    pub on_pool_exhausted: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CircuitBreakerConfig {
    pub failure_threshold: u32,
    pub cooldown_seconds: u64,
    pub cooldown_max_seconds: u64,
    pub cooldown_multiplier: f64,
    pub request_timeout_seconds: u64,
    pub health_check_interval_seconds: u64,
    pub health_check_failures: u32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct StreamingConfig {
    pub mid_stream_cutoff: bool,
    pub hard_max_output_tokens: u64,
    pub max_context_tokens: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ModelConfig {
    pub name: String,
    pub description: String,
    pub price: f64,
    pub max_context_tokens: u64,
    pub max_output_tokens: u64,
    pub supports_vision: bool,
    pub supports_thinking: bool,
    pub billing_basis: String,
    pub rates: ModelRates,
    #[serde(default)]
    pub endpoints: Vec<ModelEndpoint>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ModelRates {
    pub cache_read_offpeak: f64,
    pub cache_read_peak: f64,
    pub input_offpeak: f64,
    pub input_peak: f64,
    pub output_offpeak: f64,
    pub output_peak: f64,
}

#[derive(Debug, Clone, Deserialize)]
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
    /// cover the DEAREST one (docs/failover.md:162-165); without a per-endpoint
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
    /// (docs/failover.md:162-165). Each endpoint is priced at its own peak rates
    /// when it overrides them, else the model's; a model with no endpoints
    /// registered reserves at the model rate, exactly as
    /// `UpstreamClient::worst_case_reservation_idr` does.
    ///
    /// This lives here rather than inline in the handler because the handler and
    /// the upstream client previously each carried their own copy, and a rule
    /// written twice is a rule that can disagree with itself.
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

        self.endpoints
            .iter()
            .map(|endpoint| {
                at(
                    endpoint.effective_input_peak(self),
                    endpoint.effective_output_peak(self),
                )
            })
            .max()
            .unwrap_or_else(|| at(self.rates.input_peak, self.rates.output_peak))
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

        if self.models.is_empty() {
            return Err("At least one model must be configured in models".into());
        }
        for model in &self.models {
            if model.price <= 0.0 {
                return Err(
                    format!("Model {} has invalid price multiplier <= 0", model.name).into(),
                );
            }
            if model.rates.input_peak <= 0.0 || model.rates.output_peak <= 0.0 {
                return Err(format!("Model {} is missing peak rates", model.name).into());
            }
            // An override is what the reservation is sized from, so a 0 or
            // negative one would under-reserve silently. A MISSING override
            // falls back to the model's rate, so only a present value is checked.
            for endpoint in &model.endpoints {
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
}
