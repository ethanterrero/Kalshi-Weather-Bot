mod decision_log;
mod kill_switch;
mod source_health;

use anyhow::Result;
use chrono::{DateTime, Utc};
use clap::Parser;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tracing::{info, warn};
use weather_config::{AppConfig, ExecutionMode};
use weather_forecast::{
    daily_high_stats, daily_low_stats, EnsembleForecast, GefsClient, NwsClient,
};
use weather_pricing::{price_market_with_sigma, PricingError};
use weather_risk::{RejectReason, RiskDecision, RiskManager};
use weather_scanner::MarketScanner;
use weather_strategy::{decide, Decision, FeeModel, NoTradeReason};
use weather_types::{lookup_city, CitySpec, Forecast, TempStat};

use decision_log::{record_from, DecisionLogger};
use kill_switch::{evaluate as evaluate_kill_switch, KillState};
use source_health::{classify as classify_source_freshness, SourceFreshness};

/// Multiplier of the configured refresh interval after which a source is
/// considered "stale" (broken) rather than just "needs refresh." 2 means
/// "we missed two refreshes in a row" — that's a real outage signal, not
/// a one-tick blip. Conservative; the loop does refresh-on-demand so the
/// healthy path never reaches 2× regardless.
const SOURCE_STALENESS_MULTIPLIER: u32 = 2;

/// Command-line flags accepted by the bot binary. Anything not on the CLI
/// comes from `config/default.toml` + env var overrides.
#[derive(Parser, Debug)]
#[command(
    name = "weather-bot",
    about = "Kalshi Weather Bot — strategy + decision logger",
    long_about = None,
)]
struct Cli {
    /// Run a single strategy pass and exit. Useful for cron / CI smoke
    /// tests instead of the long-lived loop. The market-list refresh and
    /// strategy pass run sequentially in this mode rather than as
    /// independent tasks, so the binary returns 0 only after both have
    /// completed.
    #[arg(long)]
    once: bool,
}

/// Cached forecast keyed on Kalshi city code (e.g. "NY"). Refreshed on its
/// own cadence; the strategy loop reads from this cache rather than calling
/// NWS per market (which would burn the rate limit and be redundant — every
/// market for a city/day shares one forecast).
type ForecastCache = Arc<RwLock<HashMap<String, (Forecast, DateTime<Utc>)>>>;

/// Same as ForecastCache but for GEFS ensemble fetches. Open-Meteo
/// returns 30 perturbed members + a control out to ~16 forecast days
/// per call, so one fetch covers every (date, stat) market we'd price
/// for that city today and well into the future.
type EnsembleCache = Arc<RwLock<HashMap<String, (EnsembleForecast, DateTime<Utc>)>>>;

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let config = AppConfig::load()?;
    init_tracing(&config.logging);

    info!(
        kalshi_env = %config.kalshi.env,
        kalshi_base = %config.kalshi.base_url(),
        nws_base = %config.forecast.nws_base_url,
        execution = %config.execution.mode,
        series_count = config.scanner.series_tickers.len(),
        once = cli.once,
        "Kalshi Weather Bot starting"
    );

    match config.execution.mode {
        ExecutionMode::Live => warn!(
            "execution.mode = live; live order placement is NOT implemented yet — \
             staying in dry-run for this run"
        ),
        ExecutionMode::Paper => warn!(
            "execution.mode = paper; paper-trade adapter is NOT implemented yet — \
             staying in dry-run for this run"
        ),
        ExecutionMode::DryRun => {}
    }

    let scanner = Arc::new(MarketScanner::new(&config));
    let nws = Arc::new(NwsClient::new(
        config.forecast.nws_base_url.clone(),
        config.forecast.user_agent.clone(),
    ));
    let gefs = Arc::new(GefsClient::open_meteo());
    let forecast_cache: ForecastCache = Arc::new(RwLock::new(HashMap::new()));
    let ensemble_cache: EnsembleCache = Arc::new(RwLock::new(HashMap::new()));

    let decision_logger = Arc::new(DecisionLogger::new(
        config
            .logging
            .decision_log_dir
            .as_deref()
            .filter(|s| !s.is_empty())
            .map(PathBuf::from),
    ));
    if let Some(dir) = config
        .logging
        .decision_log_dir
        .as_deref()
        .filter(|s| !s.is_empty())
    {
        info!(dir, "decision log enabled");
    } else {
        info!("decision log disabled (logging.decision_log_dir = null/empty)");
    }

    info!(
        kill_switch_enabled = config.kill_switch.enabled,
        kill_file_path = %config.kill_switch.kill_file_path,
        kill_env_var = %config.kill_switch.kill_env_var,
        max_drawdown_24h_usd = %config.kill_switch.max_drawdown_24h_usd,
        "kill switch configured"
    );

    info!("Running initial Kalshi market scan...");
    match scanner.refresh().await {
        Ok(count) => info!(count, "Initial scan complete"),
        Err(e) => warn!(error = %e, "Initial scan failed; continuing with empty market set"),
    }

    let mut risk = RiskManager::new(config.risk.clone());
    info!(
        max_position_size_usd = %config.risk.max_position_size_usd,
        max_total_exposure_usd = %config.risk.max_total_exposure_usd,
        max_concurrent_positions = config.risk.max_concurrent_positions,
        per_market_cooldown_secs = config.risk.per_market_cooldown_secs,
        bankroll_usd = %config.risk.bankroll_usd,
        "risk caps active"
    );
    info!(
        gefs_enabled = config.forecast.gefs_sigma_enabled,
        gefs_refresh_secs = config.forecast.gefs_refresh_interval_secs,
        "GEFS σ source"
    );

    if cli.once {
        info!("--once: running a single strategy pass then exiting");
        run_one_pass(
            &scanner,
            &nws,
            &gefs,
            &forecast_cache,
            &ensemble_cache,
            &config,
            &decision_logger,
            &mut risk,
        )
        .await;
        info!("--once pass complete; exiting");
        return Ok(());
    }

    // Periodic market-list refresh.
    let scanner_for_loop = scanner.clone();
    let poll_ms = config.monitor.poll_interval_ms.max(5_000);
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_millis(poll_ms));
        interval.tick().await;
        loop {
            interval.tick().await;
            if let Err(e) = scanner_for_loop.refresh().await {
                warn!(error = %e, "Periodic market scan failed");
            }
        }
    });

    // Strategy loop: every `poll_ms`, walk the tracked-markets snapshot, refresh
    // forecasts for any city we don't have a recent one for, and emit a decision
    // log line per market.
    let scanner_for_strat = scanner.clone();
    let cfg_for_strat = config.clone();
    let nws_for_strat = nws.clone();
    let gefs_for_strat = gefs.clone();
    let cache_for_strat = forecast_cache.clone();
    let ensemble_for_strat = ensemble_cache.clone();
    let logger_for_strat = decision_logger.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_millis(poll_ms));
        interval.tick().await;
        loop {
            interval.tick().await;
            run_one_pass(
                &scanner_for_strat,
                &nws_for_strat,
                &gefs_for_strat,
                &cache_for_strat,
                &ensemble_for_strat,
                &cfg_for_strat,
                &logger_for_strat,
                &mut risk,
            )
            .await;
        }
    });

    info!("Strategy loop running; SIGINT/SIGTERM to exit");
    wait_for_shutdown_signal().await;
    info!("Shutting down");
    Ok(())
}

/// Wait for either SIGINT (ctrl+C) or SIGTERM, whichever lands first.
/// SIGTERM is what systemd sends on `systemctl stop`, so handling it
/// matches the dynamic-kill-switch story: an operator should be able to
/// halt the bot in seconds without redeploying. On non-Unix platforms
/// SIGTERM is unavailable; we fall back to SIGINT alone.
async fn wait_for_shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut term = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            Err(e) => {
                warn!(error = %e, "failed to install SIGTERM handler; falling back to ctrl+C only");
                let _ = tokio::signal::ctrl_c().await;
                return;
            }
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => info!(signal = "SIGINT", "shutdown signal received"),
            _ = term.recv() => info!(signal = "SIGTERM", "shutdown signal received"),
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
        info!(signal = "SIGINT", "shutdown signal received");
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_one_pass(
    scanner: &MarketScanner,
    nws: &NwsClient,
    gefs: &GefsClient,
    cache: &ForecastCache,
    ensemble_cache: &EnsembleCache,
    cfg: &AppConfig,
    logger: &DecisionLogger,
    risk: &mut RiskManager,
) {
    // Dynamic kill switch: cheapest possible check, runs every pass.
    // `realised_loss_24h_usd = None` until the executor lands a fill
    // ledger; the soft kill stays inert in dry-run regardless.
    match evaluate_kill_switch(&cfg.kill_switch, None) {
        KillState::Active => {}
        KillState::Halted(reason) => {
            warn!(
                kill_reason = reason.tag(),
                kill_detail = %reason,
                "kill switch active; skipping strategy pass"
            );
            return;
        }
    }

    run_strategy_pass(scanner, nws, gefs, cache, ensemble_cache, cfg, logger, risk).await;
}

/// Read tracked markets, refresh forecasts on demand, run pricing + EV gate
/// per market, and log the decision.
#[allow(clippy::too_many_arguments)]
async fn run_strategy_pass(
    scanner: &MarketScanner,
    nws: &NwsClient,
    gefs: &GefsClient,
    cache: &ForecastCache,
    ensemble_cache: &EnsembleCache,
    cfg: &AppConfig,
    logger: &DecisionLogger,
    risk: &mut RiskManager,
) {
    let snapshot = {
        let lock = scanner.markets();
        let guard = lock.read().await;
        guard.clone()
    };
    if snapshot.is_empty() {
        return;
    }

    let scanned = snapshot.len();
    let mut summary = PassSummary::default();

    // Each pass starts with a clean risk tally — dry-run has no fill
    // confirmations to persist between passes. When live trading lands this
    // gets replaced with a snapshot pulled from /portfolio/positions.
    risk.reset_for_pass();

    let fees = FeeModel {
        multiplier: cfg.strategy.fee_multiplier,
    };
    let refresh_after = Duration::from_secs(cfg.forecast.refresh_interval_secs);
    let ensemble_refresh_after = Duration::from_secs(cfg.forecast.gefs_refresh_interval_secs);

    for tracked in snapshot {
        let city_code = &tracked.threshold.city;
        let Some(city) = lookup_city(city_code) else {
            warn!(city = city_code, ticker = %tracked.market.ticker,
                "skipping: city not in mapping table; cannot validate NWS settlement station");
            summary.unknown_city += 1;
            continue;
        };

        // Refresh-on-demand: if we don't have a forecast for this city or it's
        // older than refresh_interval_secs, fetch a fresh one. NWS rate-limits
        // shared User-Agents so we keep it serial here.
        let cached_ts = {
            let read = cache.read().await;
            read.get(city_code).map(|(_, ts)| *ts)
        };
        let needs_refresh = match cached_ts {
            None => true,
            Some(ts) => {
                Utc::now()
                    .signed_duration_since(ts)
                    .to_std()
                    .unwrap_or_default()
                    > refresh_after
            }
        };
        let mut refresh_failed = false;
        if needs_refresh {
            match nws.fetch_point_forecast(city.lat, city.lon).await {
                Ok(f) => {
                    let mut write = cache.write().await;
                    write.insert(city_code.clone(), (f, Utc::now()));
                }
                Err(e) => {
                    warn!(city = city_code, error = %e, "NWS forecast fetch failed");
                    refresh_failed = true;
                }
            }
        }

        // After (any) refresh attempt, classify the cached value's
        // freshness. If it's past 2× the refresh interval AND we just
        // failed to fix it, treat the source as broken — log distinctly
        // and skip rather than serve stale data.
        if refresh_failed {
            let freshness = classify_source_freshness(
                cached_ts,
                refresh_after,
                SOURCE_STALENESS_MULTIPLIER,
                Utc::now(),
            );
            if let SourceFreshness::Stale { age, threshold } = freshness {
                warn!(
                    source = "nws",
                    city = city_code,
                    age_secs = age.as_secs(),
                    threshold_secs = threshold.as_secs(),
                    "source_stale: skipping market — NWS cache past staleness threshold and refresh failed"
                );
                summary.nws_source_stale += 1;
                continue;
            }
            // Stale-but-not-yet-stale-enough OR cold-start — count as a
            // plain fetch failure (existing behaviour).
            summary.forecast_fetch_failed += 1;
            continue;
        }

        let forecast = {
            let read = cache.read().await;
            match read.get(city_code) {
                Some((f, _)) => f.clone(),
                None => {
                    summary.forecast_fetch_failed += 1;
                    continue;
                }
            }
        };

        // Optional GEFS ensemble σ override. Refresh-on-demand similar to
        // NWS, but on its own (longer) cadence — ensemble runs only update
        // every 6h. Failures fall back silently to the static σ table; no
        // reason to abandon the trade just because one source is down.
        let sigma_override = if cfg.forecast.gefs_sigma_enabled {
            let stale =
                ensure_ensemble_fresh(gefs, ensemble_cache, city, ensemble_refresh_after).await;
            if stale {
                summary.gefs_source_stale += 1;
            }
            resolve_gefs_sigma(ensemble_cache, city, &tracked.threshold).await
        } else {
            None
        };

        let pricing = match price_market_with_sigma(&tracked.threshold, &forecast, sigma_override) {
            Ok(p) => p,
            Err(PricingError::NoMatchingForecastPeriod) => {
                // Common case: market is more than 7 days out (NWS horizon).
                summary.outside_horizon += 1;
                continue;
            }
            Err(e) => {
                warn!(ticker = %tracked.market.ticker, error = %e, "pricing failed");
                summary.pricing_failed += 1;
                continue;
            }
        };
        summary.priced += 1;

        // NWS-update lockout: if the forecast we're pricing against was
        // issued in the last `nws_lockout_after_update_secs`, sit out the
        // trade — arbitrage bots reprice within seconds of an issue and
        // we don't want to be on the wrong side of that. The freshness
        // clock is NWS's `generatedAt`, not our local fetch time.
        let decision =
            match nws_lockout_decision(&forecast, cfg.forecast.nws_lockout_after_update_secs) {
                Some(reason) => Decision::NoTrade(reason),
                None => decide(
                    &tracked.market,
                    &pricing,
                    &cfg.strategy,
                    &fees,
                    cfg.risk.bankroll_usd,
                ),
            };
        summary.tally_decision(&decision);

        let record = record_from(
            &tracked.market,
            &tracked.threshold,
            &pricing,
            &decision,
            cfg.execution.mode,
            Utc::now(),
        );
        if let Err(e) = logger.record(&record).await {
            warn!(error = %e, ticker = %tracked.market.ticker, "decision log write failed");
        }

        log_decision(&tracked.market.ticker, &pricing, &decision);

        // Run Trade signals through the risk layer. Position-size and
        // total-exposure caps may clip the contract count; the concurrent-
        // positions cap may reject outright; the per-market cooldown may
        // reject if we just emitted a signal for this market.
        if let Decision::Trade(sig, _) = &decision {
            if let Some(reject) = apply_risk(risk, sig) {
                summary.tally_risk_reject(&reject);
            }
        }
    }

    summary.emit(scanned);
}

/// Per-strategy-pass tally. One INFO log line at end-of-pass so the
/// operator can read a single line per cycle instead of counting
/// individual decision lines. Fields line up roughly with the JSONL
/// `reason` tags so an `awk`/`jq` consumer can cross-check.
#[derive(Debug, Default)]
struct PassSummary {
    priced: usize,
    traded: usize,
    no_orderbook: usize,
    spread_too_wide: usize,
    edge_below_min: usize,
    ev_below_gate: usize,
    price_out_of_band: usize,
    forecast_too_fresh: usize,
    /// Markets skipped before pricing — kept separately so `priced` is a
    /// clean denominator for "% of priced markets that traded".
    unknown_city: usize,
    outside_horizon: usize,
    forecast_fetch_failed: usize,
    pricing_failed: usize,
    /// Risk-layer rejections. These are post-`Decision::Trade` and don't
    /// reduce `traded`; they tell us how much of the strategy's intent
    /// the risk manager filtered out.
    risk_in_cooldown: usize,
    risk_concurrent_capped: usize,
    risk_no_budget: usize,
    /// NWS forecasts skipped because the cached value crossed the
    /// staleness threshold and a refresh attempt failed. One per market
    /// affected — operator should grep `source_stale` log lines for
    /// the underlying error rate.
    nws_source_stale: usize,
    /// GEFS ensemble σ skipped because the cached run is stale. Falls
    /// back to the static σ table; not a fatal condition. Counted
    /// separately so the operator can tell whether the bot has been
    /// running on the static table all day.
    gefs_source_stale: usize,
}

impl PassSummary {
    fn tally_decision(&mut self, decision: &Decision) {
        match decision {
            Decision::Trade(_, _) => self.traded += 1,
            Decision::NoTrade(reason) => match reason {
                NoTradeReason::NoOrderbook => self.no_orderbook += 1,
                NoTradeReason::SpreadTooWide { .. } => self.spread_too_wide += 1,
                NoTradeReason::EdgeBelowMin { .. } => self.edge_below_min += 1,
                NoTradeReason::EvBelowGate { .. } => self.ev_below_gate += 1,
                NoTradeReason::PriceOutOfBand { .. } => self.price_out_of_band += 1,
                NoTradeReason::ForecastTooFresh { .. } => self.forecast_too_fresh += 1,
            },
        }
    }

    fn tally_risk_reject(&mut self, reject: &RejectReason) {
        match reject {
            RejectReason::InCooldown { .. } => self.risk_in_cooldown += 1,
            RejectReason::ConcurrentPositionsCapped => self.risk_concurrent_capped += 1,
            RejectReason::NoBudgetRemaining => self.risk_no_budget += 1,
        }
    }

    fn emit(&self, scanned: usize) {
        info!(
            scanned,
            priced = self.priced,
            traded = self.traded,
            no_orderbook = self.no_orderbook,
            spread_too_wide = self.spread_too_wide,
            edge_below_min = self.edge_below_min,
            ev_below_gate = self.ev_below_gate,
            price_out_of_band = self.price_out_of_band,
            forecast_too_fresh = self.forecast_too_fresh,
            unknown_city = self.unknown_city,
            outside_horizon = self.outside_horizon,
            forecast_fetch_failed = self.forecast_fetch_failed,
            pricing_failed = self.pricing_failed,
            risk_in_cooldown = self.risk_in_cooldown,
            risk_concurrent_capped = self.risk_concurrent_capped,
            risk_no_budget = self.risk_no_budget,
            nws_source_stale = self.nws_source_stale,
            gefs_source_stale = self.gefs_source_stale,
            "strategy pass complete"
        );
    }
}

/// Refresh the cached GEFS ensemble for `city` if it's missing or stale.
/// Failures are logged at warn but do not propagate — the strategy loop
/// falls back to the static σ table whenever no fresh ensemble is
/// available.
///
/// Returns `true` when the cache crossed the staleness threshold AND the
/// most recent refresh attempt failed. The caller treats this as
/// "ensemble σ is unavailable" (falls back to static) and increments the
/// `gefs_source_stale` counter.
async fn ensure_ensemble_fresh(
    gefs: &GefsClient,
    cache: &EnsembleCache,
    city: &CitySpec,
    refresh_after: Duration,
) -> bool {
    let cached_ts = {
        let read = cache.read().await;
        read.get(city.kalshi_code).map(|(_, ts)| *ts)
    };
    let needs_refresh = match cached_ts {
        None => true,
        Some(ts) => {
            Utc::now()
                .signed_duration_since(ts)
                .to_std()
                .unwrap_or_default()
                > refresh_after
        }
    };
    if !needs_refresh {
        return false;
    }
    // 7 forecast days covers everything NWS prices today and gives the
    // pricing layer a horizon match for any in-horizon market.
    match gefs.fetch_gfs05(city.lat, city.lon, 7).await {
        Ok(f) => {
            let mut write = cache.write().await;
            write.insert(city.kalshi_code.to_string(), (f, Utc::now()));
            false
        }
        Err(e) => {
            let freshness = classify_source_freshness(
                cached_ts,
                refresh_after,
                SOURCE_STALENESS_MULTIPLIER,
                Utc::now(),
            );
            if let SourceFreshness::Stale { age, threshold } = freshness {
                warn!(
                    source = "gefs",
                    city = city.kalshi_code,
                    error = %e,
                    age_secs = age.as_secs(),
                    threshold_secs = threshold.as_secs(),
                    "source_stale: GEFS cache past staleness threshold and refresh failed; falling back to static σ"
                );
                true
            } else {
                warn!(city = city.kalshi_code, error = %e, "GEFS ensemble fetch failed; falling back to static σ");
                false
            }
        }
    }
}

/// Pull the cached GEFS ensemble for `city` and derive σ for the market's
/// (date, stat). Returns `None` whenever no in-window ensemble data is
/// available — the caller falls back to the static σ table.
async fn resolve_gefs_sigma(
    cache: &EnsembleCache,
    city: &CitySpec,
    threshold: &weather_types::WeatherThreshold,
) -> Option<f64> {
    let read = cache.read().await;
    let (forecast, _) = read.get(city.kalshi_code)?;
    let stat = match threshold.stat {
        TempStat::DailyHigh => daily_high_stats(forecast, city, threshold.date)?,
        TempStat::DailyLow => daily_low_stats(forecast, city, threshold.date)?,
    };
    // A handful of members can produce a zero or near-zero σ that's just
    // sample noise. Guard against that by ignoring tiny σ — we'd rather
    // fall back to the static table than over-confidently price near 0/1.
    if stat.n_members < 5 || stat.sigma_f < 0.25 {
        return None;
    }
    Some(stat.sigma_f)
}

/// If the NWS forecast was issued less than `lockout_secs` ago, return
/// the matching `NoTradeReason::ForecastTooFresh`. `lockout_secs == 0` or
/// a forecast missing `generated_at` both disable the gate.
fn nws_lockout_decision(forecast: &Forecast, lockout_secs: u64) -> Option<NoTradeReason> {
    if lockout_secs == 0 {
        return None;
    }
    let issued = forecast.generated_at?;
    let age = Utc::now().signed_duration_since(issued).num_seconds();
    let lockout = lockout_secs as i64;
    if (0..lockout).contains(&age) {
        Some(NoTradeReason::ForecastTooFresh {
            age_secs: age,
            lockout_secs: lockout,
        })
    } else {
        None
    }
}

/// Apply risk caps to a strategy signal. Returns the reject reason (if any)
/// so the per-pass summary can tally how often each cap fires.
fn apply_risk(risk: &mut RiskManager, signal: &weather_types::Signal) -> Option<RejectReason> {
    let original_contracts = signal.contracts;
    match risk.evaluate(signal.clone()) {
        RiskDecision::Approve(s) => {
            info!(
                ticker = %s.market_ticker,
                contracts = s.contracts,
                limit_price = %s.limit_price,
                pass_exposure_usd = %risk.pass_exposure_usd(),
                "risk: approved"
            );
            None
        }
        RiskDecision::Adjusted(s, reason) => {
            info!(
                ticker = %s.market_ticker,
                contracts = s.contracts,
                original_contracts,
                limit_price = %s.limit_price,
                reason = ?reason,
                pass_exposure_usd = %risk.pass_exposure_usd(),
                "risk: clipped position size"
            );
            None
        }
        RiskDecision::Reject(reason) => {
            info!(
                ticker = %signal.market_ticker,
                reason = ?reason,
                pass_exposure_usd = %risk.pass_exposure_usd(),
                pass_position_count = risk.pass_position_count(),
                "risk: rejected"
            );
            Some(reason)
        }
    }
}

fn log_decision(ticker: &str, pricing: &weather_pricing::ModelPricing, decision: &Decision) {
    match decision {
        Decision::Trade(sig, ev) => {
            info!(
                ticker,
                side = ?sig.side,
                limit_price = %sig.limit_price,
                contracts = sig.contracts,
                model_p = %ev.model_probability,
                market_p = %ev.market_implied_probability,
                spread = %ev.spread,
                fee_est = %ev.fee_estimate,
                raw_edge = %ev.raw_edge,
                net_ev = %ev.net_ev_per_contract,
                station = pricing.settlement_station,
                forecast_temp_f = pricing.forecast_temp_f,
                horizon_days = pricing.horizon_days,
                sigma_f = pricing.sigma_f,
                "TRADE (dry-run)"
            );
        }
        Decision::NoTrade(reason) => match reason {
            // Quietest reason — most markets don't have a tradable edge.
            NoTradeReason::EdgeBelowMin { .. } | NoTradeReason::EvBelowGate { .. } => {
                tracing::debug!(
                    ticker,
                    reason = %reason,
                    model_p = %pricing.yes_probability,
                    forecast_temp_f = pricing.forecast_temp_f,
                    "no-trade"
                );
            }
            _ => {
                info!(ticker, reason = %reason, "no-trade");
            }
        },
    }
}

fn init_tracing(config: &weather_config::LoggingConfig) {
    use tracing_subscriber::{fmt, EnvFilter};
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(&config.level));
    if config.json_output {
        fmt().with_env_filter(filter).json().init();
    } else {
        fmt().with_env_filter(filter).with_target(true).init();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn forecast_with(generated_at: Option<DateTime<Utc>>) -> Forecast {
        Forecast {
            lat: 40.78,
            lon: -73.97,
            fetched_at: Utc::now(),
            generated_at,
            periods: Vec::new(),
        }
    }

    #[test]
    fn lockout_zero_seconds_disables_the_gate() {
        let f = forecast_with(Some(Utc::now()));
        assert!(nws_lockout_decision(&f, 0).is_none());
    }

    #[test]
    fn missing_generated_at_disables_the_gate() {
        let f = forecast_with(None);
        assert!(nws_lockout_decision(&f, 1800).is_none());
    }

    #[test]
    fn fresh_forecast_inside_window_triggers_lockout() {
        // Issued 5 minutes ago, lockout is 30 minutes.
        let issued = Utc::now() - chrono::Duration::minutes(5);
        let f = forecast_with(Some(issued));
        match nws_lockout_decision(&f, 1800) {
            Some(NoTradeReason::ForecastTooFresh {
                age_secs,
                lockout_secs,
            }) => {
                assert!((290..=310).contains(&age_secs), "age: {}", age_secs);
                assert_eq!(lockout_secs, 1800);
            }
            other => panic!("expected ForecastTooFresh, got {:?}", other),
        }
    }

    #[test]
    fn stale_forecast_outside_window_does_not_trigger_lockout() {
        // Issued 90 minutes ago, lockout is 30 minutes.
        let issued = Utc::now() - chrono::Duration::minutes(90);
        let f = forecast_with(Some(issued));
        assert!(nws_lockout_decision(&f, 1800).is_none());
    }

    #[test]
    fn forecast_issued_in_the_future_does_not_trigger_lockout() {
        // Defensive: future timestamps shouldn't lock us out forever.
        let issued = Utc::now() + chrono::Duration::minutes(10);
        let f = forecast_with(Some(issued));
        assert!(nws_lockout_decision(&f, 1800).is_none());
    }

    // Pass-summary tally tests. We avoid constructing real Decision::Trade
    // values (the EvBreakdown is verbose) and instead exercise NoTrade for
    // each variant; the Trade path is one line in tally_decision and is
    // covered transitively by the strategy crate's own Trade tests.
    use rust_decimal_macros::dec;

    #[test]
    fn pass_summary_starts_at_zero() {
        let s = PassSummary::default();
        assert_eq!(s.priced, 0);
        assert_eq!(s.traded, 0);
        assert_eq!(s.edge_below_min, 0);
    }

    #[test]
    fn tally_decision_increments_per_no_trade_variant() {
        let mut s = PassSummary::default();
        s.tally_decision(&Decision::NoTrade(NoTradeReason::NoOrderbook));
        s.tally_decision(&Decision::NoTrade(NoTradeReason::SpreadTooWide {
            spread: dec!(0.20),
            max: dec!(0.10),
        }));
        s.tally_decision(&Decision::NoTrade(NoTradeReason::EdgeBelowMin {
            raw_edge: dec!(0.02),
            min: dec!(0.05),
        }));
        s.tally_decision(&Decision::NoTrade(NoTradeReason::EvBelowGate {
            net_ev: dec!(0.005),
            required: dec!(0.01),
        }));
        s.tally_decision(&Decision::NoTrade(NoTradeReason::PriceOutOfBand {
            price: dec!(0.05),
            min: dec!(0.20),
            max: dec!(0.92),
        }));
        s.tally_decision(&Decision::NoTrade(NoTradeReason::ForecastTooFresh {
            age_secs: 120,
            lockout_secs: 1800,
        }));

        assert_eq!(s.no_orderbook, 1);
        assert_eq!(s.spread_too_wide, 1);
        assert_eq!(s.edge_below_min, 1);
        assert_eq!(s.ev_below_gate, 1);
        assert_eq!(s.price_out_of_band, 1);
        assert_eq!(s.forecast_too_fresh, 1);
        assert_eq!(s.traded, 0);
    }

    #[test]
    fn tally_decision_groups_repeats_into_one_counter() {
        let mut s = PassSummary::default();
        for _ in 0..5 {
            s.tally_decision(&Decision::NoTrade(NoTradeReason::EdgeBelowMin {
                raw_edge: dec!(0.02),
                min: dec!(0.05),
            }));
        }
        assert_eq!(s.edge_below_min, 5);
    }
}
