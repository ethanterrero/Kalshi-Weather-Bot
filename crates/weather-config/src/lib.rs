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
    /// Skip trades for `nws_lockout_after_update_secs` after the NWS
    /// forecast was *issued* (its `generatedAt`, not our fetch time).
    /// Arbitrage bots reprice within seconds of an NWS update; sitting out
    /// the first 30 minutes avoids being adversely selected. Default 1800
    /// seconds. Set to 0 to disable.
    #[serde(default = "default_nws_lockout_secs")]
    pub nws_lockout_after_update_secs: u64,
}

fn default_nws_lockout_secs() -> u64 {
    1800
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
    /// Extra margin (dollars per contract) the EV gate requires on top of
    /// half-spread + estimated fee before taking a position. 0 = pure
    /// fee+spread breakeven. Default 0.01 = 1¢ buffer.
    #[serde(default = "default_safety_buffer")]
    pub safety_buffer: Decimal,
    /// Per-100-contracts fee multiplier the fee model assumes. Most Kalshi
    /// markets are 1.0; some series are higher. Default matches the
    /// help-center fee schedule.
    #[serde(default = "default_fee_multiplier")]
    pub fee_multiplier: Decimal,
    /// Maximum bid-ask spread (in dollars) we'll trade across. Default 0.10.
    #[serde(default = "default_max_spread")]
    pub max_spread: Decimal,
    /// Lowest price we'll pay per contract. Below this, fees are a fatal
    /// fraction of the price and one losing tail event wipes out months of
    /// small wins. Default 0.20.
    #[serde(default = "default_min_price")]
    pub min_price: Decimal,
    /// Highest price we'll pay per contract. At/above this the implied
    /// edge has to be huge to clear fees + asymmetric tail risk. Default
    /// 0.92.
    #[serde(default = "default_max_price")]
    pub max_price: Decimal,
}

fn default_safety_buffer() -> Decimal {
    Decimal::new(1, 2)
}
fn default_fee_multiplier() -> Decimal {
    Decimal::ONE
}
fn default_max_spread() -> Decimal {
    Decimal::new(10, 2)
}
fn default_min_price() -> Decimal {
    Decimal::new(20, 2)
}
fn default_max_price() -> Decimal {
    Decimal::new(92, 2)
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
    /// Directory for the per-day JSONL decision log (one row per market per
    /// strategy pass). `None` disables persistence; default is
    /// `logs/decisions`. Backtests and calibration both feed off this file.
    #[serde(default = "default_decision_log_dir")]
    pub decision_log_dir: Option<String>,
}

fn default_decision_log_dir() -> Option<String> {
    Some("logs/decisions".to_string())
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
