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
    /// No order placement. Strategy logs decisions only.
    DryRun,
    /// Same code path as `Live` but routed through a paper-trade endpoint
    /// (or local mock simulating fills against the real orderbook).
    /// Surfaces lifecycle bugs that dry-run can't catch.
    Paper,
    /// Real money. Requires `KALSHI_ENV=prod` and the executor's
    /// `never_send` to be explicitly disabled in code.
    Live,
}

impl ExecutionMode {
    /// Stable lowercase string used in structured logs and the JSONL
    /// decision record. Backtests/replay split metrics on this so dry-run,
    /// paper, and live data don't get pooled.
    pub fn as_str(&self) -> &'static str {
        match self {
            ExecutionMode::DryRun => "dry_run",
            ExecutionMode::Paper => "paper",
            ExecutionMode::Live => "live",
        }
    }
}

impl std::fmt::Display for ExecutionMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
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
    /// Dynamic kill switch — a way for the operator to halt the bot in
    /// seconds without redeploying. Distinct from the executor's static
    /// `never_send=true` (which requires a code change).
    #[serde(default)]
    pub kill_switch: KillSwitchConfig,
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
    /// Use GEFS ensemble σ in pricing instead of the hand-calibrated
    /// `sigma_for_horizon` table. The bot keeps the static table as a
    /// fallback whenever the ensemble fetch fails or no in-window members
    /// are available, so flipping this on is safe. Default true.
    #[serde(default = "default_gefs_sigma_enabled")]
    pub gefs_sigma_enabled: bool,
    /// How often to refresh the GEFS ensemble (seconds). GEFS only
    /// publishes new runs every ~6h, so polling more frequently is
    /// pointless. Default 1800 = 30 min, which gives us at most one
    /// stale window per real run.
    #[serde(default = "default_gefs_refresh_secs")]
    pub gefs_refresh_interval_secs: u64,
    /// Use ECMWF IFS-0.25° ensemble σ in pricing. When `true` and GEFS is
    /// also enabled, the bot pools per-member daily extremes from both
    /// ensembles and computes a blended `(μ, σ)` — the JSONL stamps
    /// `sigma_source = "gefs_ecmwf_blend"` so the backtester can split
    /// metrics. When `true` and GEFS is disabled, ECMWF alone is used
    /// (tag `"ecmwf_ensemble"`). Fallback to static σ on fetch failure
    /// matches the GEFS path. Default true.
    #[serde(default = "default_ecmwf_sigma_enabled")]
    pub ecmwf_sigma_enabled: bool,
    /// How often to refresh the ECMWF ensemble (seconds). ECMWF publishes
    /// new runs every ~6h on the same cadence as GEFS; 1800 = 30 min.
    #[serde(default = "default_ecmwf_refresh_secs")]
    pub ecmwf_refresh_interval_secs: u64,
    /// Use intraday METAR observations to *lock* YES on settlement-day
    /// markets whose running daily-high (resp. daily-low) already crossed
    /// the strike. CLI reports the day's max/min, which is monotone — a
    /// valid in-window observation past the strike means YES will resolve
    /// true regardless of what happens later in the day. Default true.
    #[serde(default = "default_intraday_lock_enabled")]
    pub intraday_lock_enabled: bool,
    /// How often to refresh METAR observations (seconds). NWS publishes
    /// 5-minute SPECI between hourly METARs at major airports; an edge
    /// `s-maxage=300` cache fronts that. 300 = match the edge cache
    /// without thrashing.
    #[serde(default = "default_metar_refresh_secs")]
    pub metar_refresh_interval_secs: u64,
}

fn default_nws_lockout_secs() -> u64 {
    1800
}

fn default_gefs_sigma_enabled() -> bool {
    true
}

fn default_gefs_refresh_secs() -> u64 {
    1800
}

fn default_ecmwf_sigma_enabled() -> bool {
    true
}

fn default_ecmwf_refresh_secs() -> u64 {
    1800
}

fn default_intraday_lock_enabled() -> bool {
    true
}

fn default_metar_refresh_secs() -> u64 {
    300
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
    /// Notional bankroll the strategy's Kelly fraction sizes against.
    /// Quarter-Kelly of `bankroll_usd * (edge/odds)` is the contract-count
    /// hint; the per-market and per-pass dollar caps then clamp it. Without
    /// this, Kelly is computed against an implicit $1 and the cap does all
    /// the sizing work.
    #[serde(default = "default_bankroll_usd")]
    pub bankroll_usd: Decimal,
}

fn default_bankroll_usd() -> Decimal {
    // $100 bankroll = an intentional toy default. Override per-operator.
    Decimal::from(100)
}

/// File-flag + env-var + drawdown soft-kill bundle.
///
/// Each check is cheap and fails in a different way:
///   - `kill_file_path` — operator drops a file on disk, bot halts on the
///     next pass; survives bot restart unless removed.
///   - `kill_env_var` — set in the systemd unit before SIGHUP; useful when
///     the operator can't write to the working directory.
///   - `max_drawdown_24h_usd` — bot halts itself after a configured loss
///     within 24h. Static kill is the operator protecting the bot; this is
///     the bot protecting the operator from a bad day.
///
/// SIGTERM cancellation is handled in the bot loop, not configured here:
/// it always cancels in-flight work and exits cleanly when received.
#[derive(Debug, Clone, Deserialize)]
pub struct KillSwitchConfig {
    /// Master switch. When `false`, the file/env/drawdown checks are
    /// skipped entirely (SIGTERM still works). Defaults to `true` so a
    /// fresh deploy is safe by default.
    #[serde(default = "default_kill_switch_enabled")]
    pub enabled: bool,
    /// Path watched once per strategy pass. Existence (regardless of
    /// content) halts new orders. Default `./KILL` — relative to the
    /// bot's CWD.
    #[serde(default = "default_kill_file_path")]
    pub kill_file_path: String,
    /// Environment variable inspected once per pass. Any non-empty value
    /// other than `0`/`false` halts new orders. Default
    /// `WEATHER_BOT_KILL`.
    #[serde(default = "default_kill_env_var")]
    pub kill_env_var: String,
    /// Maximum cumulative realised loss (in USD) inside a 24h rolling
    /// window before the bot refuses new orders. `0` or negative disables
    /// the soft kill. Default `0` (disabled) until live trading lands;
    /// the dry-run bot has no realised P&L yet to enforce against.
    #[serde(default = "default_max_drawdown_usd")]
    pub max_drawdown_24h_usd: Decimal,
}

impl Default for KillSwitchConfig {
    fn default() -> Self {
        Self {
            enabled: default_kill_switch_enabled(),
            kill_file_path: default_kill_file_path(),
            kill_env_var: default_kill_env_var(),
            max_drawdown_24h_usd: default_max_drawdown_usd(),
        }
    }
}

fn default_kill_switch_enabled() -> bool {
    true
}
fn default_kill_file_path() -> String {
    "./KILL".to_string()
}
fn default_kill_env_var() -> String {
    "WEATHER_BOT_KILL".to_string()
}
fn default_max_drawdown_usd() -> Decimal {
    Decimal::ZERO
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn execution_mode_str_round_trip_matches_serde_tag() {
        assert_eq!(ExecutionMode::DryRun.as_str(), "dry_run");
        assert_eq!(ExecutionMode::Paper.as_str(), "paper");
        assert_eq!(ExecutionMode::Live.as_str(), "live");
    }

    #[test]
    fn execution_mode_display_uses_stable_lowercase_tag() {
        assert_eq!(format!("{}", ExecutionMode::DryRun), "dry_run");
        assert_eq!(format!("{}", ExecutionMode::Paper), "paper");
        assert_eq!(format!("{}", ExecutionMode::Live), "live");
    }

    #[test]
    fn kill_switch_default_is_safe_and_dry_run_flavoured() {
        let k = KillSwitchConfig::default();
        assert!(k.enabled);
        assert_eq!(k.kill_file_path, "./KILL");
        assert_eq!(k.kill_env_var, "WEATHER_BOT_KILL");
        // Drawdown soft-kill is disabled by default — dry-run has no
        // realised P&L to enforce against, so a non-zero default would
        // be an immediate no-op or worse, false-positive.
        assert_eq!(k.max_drawdown_24h_usd, Decimal::ZERO);
    }
}
