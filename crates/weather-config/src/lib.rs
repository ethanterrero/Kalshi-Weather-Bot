//! Application configuration loader. Reads `config/default.toml` and overlays
//! `__`-separated env vars (e.g. `STRATEGY__MIN_EDGE=0.07`).

use rust_decimal::Decimal;
use serde::Deserialize;
use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("failed to load configuration: {0}")]
    Load(#[from] config::ConfigError),
    #[error("missing environment variable: {0}")]
    MissingEnv(String),
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionMode {
    DryRun,
    Live,
}

fn default_execution_mode() -> ExecutionMode {
    ExecutionMode::DryRun
}

#[derive(Debug, Clone, Deserialize)]
pub struct ExecutionConfig {
    #[serde(default = "default_execution_mode")]
    pub mode: ExecutionMode,
}

impl Default for ExecutionConfig {
    fn default() -> Self {
        Self {
            mode: default_execution_mode(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct AppConfig {
    pub kalshi: KalshiConfig,
    #[serde(default)]
    pub execution: ExecutionConfig,
    pub forecast: ForecastConfig,
    pub scanner: ScannerConfig,
    pub strategy: StrategyConfig,
    pub risk: RiskConfig,
    pub monitor: MonitorConfig,
    pub logging: LoggingConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct KalshiConfig {
    /// "demo" (paper trading) or "prod" (real money).
    pub env: String,
}

impl KalshiConfig {
    /// Base URL for the Kalshi REST API matching `env`.
    pub fn base_url(&self) -> &'static str {
        match self.env.as_str() {
            "prod" => "https://trading-api.kalshi.com/trade-api/v2",
            _ => "https://demo-api.kalshi.co/trade-api/v2",
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ForecastConfig {
    pub nws_base_url: String,
    pub user_agent: String,
    pub refresh_interval_secs: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ScannerConfig {
    pub refresh_interval_secs: u64,
    /// Full Kalshi series tickers to track (e.g. `KXHIGHNY`, `KXLOWCHI`).
    /// Kalshi's `?series_ticker=` filter requires exact match, not a prefix.
    #[serde(default)]
    pub series_tickers: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct StrategyConfig {
    pub min_edge: Decimal,
    pub kelly_fraction: Decimal,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RiskConfig {
    pub max_position_size_usd: Decimal,
    pub max_total_exposure_usd: Decimal,
    pub per_market_cooldown_secs: u64,
    pub max_concurrent_positions: usize,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MonitorConfig {
    pub poll_interval_ms: u64,
    pub max_concurrent_requests: usize,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LoggingConfig {
    pub level: String,
    pub json_output: bool,
}

impl AppConfig {
    pub fn load() -> Result<Self, ConfigError> {
        Self::load_from(None)
    }

    pub fn load_from(config_path: Option<PathBuf>) -> Result<Self, ConfigError> {
        let _ = dotenvy::dotenv();
        let default_path = config_path.unwrap_or_else(|| PathBuf::from("config/default.toml"));
        let settings = config::Config::builder()
            .add_source(config::File::from(default_path).required(true))
            .add_source(
                config::Environment::default()
                    .separator("__")
                    .try_parsing(true),
            )
            .build()?;
        let cfg: AppConfig = settings.try_deserialize()?;
        Ok(cfg)
    }

    pub fn kalshi_api_key_id() -> Result<String, ConfigError> {
        std::env::var("KALSHI_API_KEY_ID")
            .map_err(|_| ConfigError::MissingEnv("KALSHI_API_KEY_ID".to_string()))
    }

    pub fn kalshi_private_key_path() -> Result<String, ConfigError> {
        std::env::var("KALSHI_PRIVATE_KEY_PATH")
            .map_err(|_| ConfigError::MissingEnv("KALSHI_PRIVATE_KEY_PATH".to_string()))
    }
}
