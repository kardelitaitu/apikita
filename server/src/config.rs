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
    pub key_metadata_cache_seconds: u64,
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
                return Err(format!("Model {} has invalid price multiplier <= 0", model.name).into());
            }
            if model.rates.input_peak <= 0.0 || model.rates.output_peak <= 0.0 {
                return Err(format!("Model {} is missing peak rates", model.name).into());
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
            .ok_or_else(|| format!("network.trusted_proxy_cidrs: {text}: expected ADDRESS/PREFIX"))?;
        let prefix: u8 = text
            .split_once('/')
            .and_then(|(_, prefix)| prefix.trim().parse().ok())
            .ok_or_else(|| format!("network.trusted_proxy_cidrs: {text}: expected ADDRESS/PREFIX"))?;

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

    #[test]
    fn test_load_apikita_toml() {
        // Test parsing the root config/apikita.toml
        let paths = ["../config/apikita.toml", "config/apikita.toml"];
        let mut loaded = false;
        for p in &paths {
            if Path::new(p).exists() {
                let config = AppConfig::load_from_file(p).expect("Failed to load apikita.toml");
                assert_eq!(config.pricing.currency, "IDR");
                assert_eq!(config.wallet.min_first_deposit, 50000);
                assert_eq!(config.wallet.min_topup, 10000);
                assert!(config.models.len() >= 2);
                loaded = true;
                break;
            }
        }
        assert!(loaded, "Could not find apikita.toml for testing");
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
            assert!(message.contains(entry), "message must name {entry}: {message}");
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
        for entry in ["203.0.113.7/32", "172.21.0.0/16", "::1/128", "2001:db8::/64"] {
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
