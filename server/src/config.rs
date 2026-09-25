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
    pub allow_negative_balance_overdraft: bool,
    pub mid_stream_cutoff: bool,
    pub default_max_output_tokens: u64,
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
}
