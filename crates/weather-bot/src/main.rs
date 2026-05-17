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
use weather_executor::{KalshiOrderClient, KalshiSigner, OrderRequest};
use weather_forecast::{
    pooled_daily_high_stats, pooled_daily_low_stats, running_high_f, running_low_f,
    EnsembleForecast, GefsClient, MetarClient, MetarObservation, NwsClient,
};
use weather_pricing::{
    price_market_with_lock, price_market_with_sigma, PricingError, SIGMA_SOURCE_ECMWF_ENSEMBLE,
    SIGMA_SOURCE_GEFS_ECMWF_BLEND, SIGMA_SOURCE_GEFS_ENSEMBLE,
};
use weather_risk::{RejectReason, RiskDecision, RiskManager};
use weather_scanner::MarketScanner;
use weather_strategy::{decide, Decision, FeeModel, NoTradeReason};
use weather_types::{
    daily_high_window_utc, daily_low_window_utc, lookup_city, CitySpec, Forecast, TempStat,
};

use decision_log::{record_from, DecisionLogger};
use kill_switch::{evaluate as evaluate_kill_switch, KillState};
use source_health::{classify as classify_source_freshness, SourceFreshness};

/// Multiplier of the configured refresh interval after which a source is
/// considered "stale" (broken) rather than just "needs refresh." 2 means
/// "we missed two refreshes in a row" — that's a real outage signal, not
/// a one-tick blip. Conservative; the loop does refresh-on-demand so the
/// healthy path never reaches 2× regardless.
const SOURCE_STALENESS_MULTIPLIER: u32 = 2;
/// Intended-order keys age out after 24h unless refreshed by a new emission.
const INTENDED_TRADE_TTL_HOURS: i64 = 24;

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

/// Same as ForecastCache but for ensemble fetches. Open-Meteo returns
/// 30 (GEFS) or 50 (ECMWF) perturbed members + a control out to ~15-16
/// forecast days per call, so one fetch covers every (date, stat) market
/// we'd price for that city today and well into the future. The bot keeps
/// one cache per source so the two are refreshed independently and the
/// pooling layer can ask "do we have either, both, or neither?" cheaply.
type EnsembleCache = Arc<RwLock<HashMap<String, (EnsembleForecast, DateTime<Utc>)>>>;

/// Which Open-Meteo ensemble a `(σ, source_tag)` comes from. Used by the
/// shared fetch + pooling helpers so the bot only encodes the GEFS-vs-ECMWF
/// distinction in one place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EnsembleSource {
    Gefs,
    Ecmwf,
}

impl EnsembleSource {
    fn label(&self) -> &'static str {
        match self {
            EnsembleSource::Gefs => "gefs",
            EnsembleSource::Ecmwf => "ecmwf",
        }
    }
}

/// METAR cache keyed on Kalshi city code. Holds the parsed observation
/// vector and the timestamp of the fetch (not the latest observation —
/// the latter is on the `MetarObservation` itself). One entry per city
/// covers every same-day market for that city.
type MetarCache = Arc<RwLock<HashMap<String, (Vec<MetarObservation>, DateTime<Utc>)>>>;
type IntendedTrades = HashMap<TradeIntentKey, DateTime<Utc>>;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct TradeIntentKey {
    ticker: String,
    side: String,
    limit_price: String,
    contracts: u32,
}

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
            "execution.mode = live; live sends remain hard-guarded unless explicitly enabled in code"
        ),
        ExecutionMode::Paper => {
            info!("execution.mode = paper; trade decisions will be handed to executor in paper-safe mode")
        }
        ExecutionMode::DryRun => {}
    }

    let scanner = Arc::new(MarketScanner::new(&config));
    let nws = Arc::new(NwsClient::new(
        config.forecast.nws_base_url.clone(),
        config.forecast.user_agent.clone(),
    ));
    let gefs = Arc::new(GefsClient::open_meteo());
    let metar = Arc::new(MetarClient::nws(config.forecast.user_agent.clone()));
    let forecast_cache: ForecastCache = Arc::new(RwLock::new(HashMap::new()));
    let gefs_cache: EnsembleCache = Arc::new(RwLock::new(HashMap::new()));
    let ecmwf_cache: EnsembleCache = Arc::new(RwLock::new(HashMap::new()));
    let metar_cache: MetarCache = Arc::new(RwLock::new(HashMap::new()));

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
    let mut intended_trades: IntendedTrades = HashMap::new();
    let mut order_client = build_order_client(&config);
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
        ecmwf_enabled = config.forecast.ecmwf_sigma_enabled,
        ecmwf_refresh_secs = config.forecast.ecmwf_refresh_interval_secs,
        "ensemble σ sources"
    );
    info!(
        intraday_lock_enabled = config.forecast.intraday_lock_enabled,
        metar_refresh_secs = config.forecast.metar_refresh_interval_secs,
        "intraday METAR lock"
    );

    if cli.once {
        info!("--once: running a single strategy pass then exiting");
        run_one_pass(
            &scanner,
            &nws,
            &gefs,
            &metar,
            &forecast_cache,
            &gefs_cache,
            &ecmwf_cache,
            &metar_cache,
            &config,
            &decision_logger,
            &mut risk,
            &mut intended_trades,
            order_client.as_mut(),
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
    let metar_for_strat = metar.clone();
    let cache_for_strat = forecast_cache.clone();
    let gefs_for_loop = gefs_cache.clone();
    let ecmwf_for_loop = ecmwf_cache.clone();
    let metar_for_loop = metar_cache.clone();
    let logger_for_strat = decision_logger.clone();
    tokio::spawn(async move {
        let mut intended_trades: IntendedTrades = HashMap::new();
        let mut order_client = build_order_client(&cfg_for_strat);
        let mut interval = tokio::time::interval(Duration::from_millis(poll_ms));
        interval.tick().await;
        loop {
            interval.tick().await;
            run_one_pass(
                &scanner_for_strat,
                &nws_for_strat,
                &gefs_for_strat,
                &metar_for_strat,
                &cache_for_strat,
                &gefs_for_loop,
                &ecmwf_for_loop,
                &metar_for_loop,
                &cfg_for_strat,
                &logger_for_strat,
                &mut risk,
                &mut intended_trades,
                order_client.as_mut(),
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
    metar: &MetarClient,
    cache: &ForecastCache,
    gefs_cache: &EnsembleCache,
    ecmwf_cache: &EnsembleCache,
    metar_cache: &MetarCache,
    cfg: &AppConfig,
    logger: &DecisionLogger,
    risk: &mut RiskManager,
    intended_trades: &mut IntendedTrades,
    order_client: Option<&mut KalshiOrderClient>,
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

    run_strategy_pass(
        scanner,
        nws,
        gefs,
        metar,
        cache,
        gefs_cache,
        ecmwf_cache,
        metar_cache,
        cfg,
        logger,
        risk,
        intended_trades,
        order_client,
    )
    .await;
}

/// Read tracked markets, refresh forecasts on demand, run pricing + EV gate
/// per market, and log the decision.
#[allow(clippy::too_many_arguments)]
async fn run_strategy_pass(
    scanner: &MarketScanner,
    nws: &NwsClient,
    gefs: &GefsClient,
    metar: &MetarClient,
    cache: &ForecastCache,
    gefs_cache: &EnsembleCache,
    ecmwf_cache: &EnsembleCache,
    metar_cache: &MetarCache,
    cfg: &AppConfig,
    logger: &DecisionLogger,
    risk: &mut RiskManager,
    intended_trades: &mut IntendedTrades,
    mut order_client: Option<&mut KalshiOrderClient>,
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
    prune_intended_trades(intended_trades);

    let fees = FeeModel {
        multiplier: cfg.strategy.fee_multiplier,
    };
    let refresh_after = Duration::from_secs(cfg.forecast.refresh_interval_secs);
    let gefs_refresh_after = Duration::from_secs(cfg.forecast.gefs_refresh_interval_secs);
    let ecmwf_refresh_after = Duration::from_secs(cfg.forecast.ecmwf_refresh_interval_secs);
    let metar_refresh_after = Duration::from_secs(cfg.forecast.metar_refresh_interval_secs);

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

        // Optional ensemble σ override. Refresh-on-demand similar to NWS,
        // but each source has its own (longer) cadence — ensemble runs
        // only update every 6h. Failures fall back silently: the
        // strategy uses whichever source is available, and falls back to
        // the static σ table when both are absent.
        if cfg.forecast.gefs_sigma_enabled {
            let stale = ensure_ensemble_fresh(
                gefs,
                gefs_cache,
                city,
                gefs_refresh_after,
                EnsembleSource::Gefs,
            )
            .await;
            if stale {
                summary.gefs_source_stale += 1;
            }
        }
        if cfg.forecast.ecmwf_sigma_enabled {
            let stale = ensure_ensemble_fresh(
                gefs,
                ecmwf_cache,
                city,
                ecmwf_refresh_after,
                EnsembleSource::Ecmwf,
            )
            .await;
            if stale {
                summary.ecmwf_source_stale += 1;
            }
        }
        let sigma_override = resolve_ensemble_sigma(
            gefs_cache,
            ecmwf_cache,
            city,
            &tracked.threshold,
            cfg.forecast.gefs_sigma_enabled,
            cfg.forecast.ecmwf_sigma_enabled,
        )
        .await;

        // Intraday lock check: only meaningful when this market settles
        // today (in city standard time) and lock is enabled. Refresh
        // METAR on demand; lock returns Some when the realised running
        // extreme has already crossed the strike. We test the lock *first*
        // because once it fires the ensemble σ is irrelevant.
        let mut lock_pricing: Option<weather_pricing::ModelPricing> = None;
        if cfg.forecast.intraday_lock_enabled && is_settlement_today(city, &tracked.threshold) {
            ensure_metar_fresh(metar, metar_cache, city, metar_refresh_after).await;
            let read = metar_cache.read().await;
            if let Some((obs, _)) = read.get(city.kalshi_code) {
                let (start, end) = match tracked.threshold.stat {
                    TempStat::DailyHigh => daily_high_window_utc(city, tracked.threshold.date),
                    TempStat::DailyLow => daily_low_window_utc(city, tracked.threshold.date),
                };
                let snap = match tracked.threshold.stat {
                    TempStat::DailyHigh => running_high_f(obs, start, end),
                    TempStat::DailyLow => running_low_f(obs, start, end),
                };
                if let Some(s) = snap {
                    match price_market_with_lock(&tracked.threshold, s.value_f) {
                        Ok(Some(p)) => {
                            lock_pricing = Some(p);
                            summary.intraday_lock_hits += 1;
                        }
                        Ok(None) => {}
                        // City-mapping error is already gated above; treat
                        // any failure here as "fall through to ensemble".
                        Err(_) => {}
                    }
                }
            }
        }

        let pricing = if let Some(p) = lock_pricing {
            p
        } else {
            match price_market_with_sigma(&tracked.threshold, &forecast, sigma_override) {
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
        let now = Utc::now();
        let mut risk_outcome: Option<&'static str> = None;
        let mut execution_outcome: Option<&'static str> = None;

        // Trade handoff: duplicate-intent guard -> risk layer -> execution.
        if let Decision::Trade(sig, _) = &decision {
            let intent_key = TradeIntentKey::from_signal(sig);
            match intended_trades.entry(intent_key) {
                std::collections::hash_map::Entry::Occupied(_) => {
                    execution_outcome = Some("suppressed_duplicate_intended");
                    summary.suppressed_duplicate_intended += 1;
                }
                std::collections::hash_map::Entry::Vacant(slot) => match apply_risk(risk, sig) {
                    RiskApplyResult::Approved {
                        signal,
                        outcome_tag,
                    } => {
                        risk_outcome = Some(outcome_tag);
                        let exec_tag =
                            execute_trade(cfg.execution.mode, order_client.as_deref_mut(), &signal)
                                .await;
                        execution_outcome = Some(exec_tag);
                        summary.tally_execution(exec_tag);
                        if should_register_intended_trade(exec_tag) {
                            slot.insert(now);
                        }
                    }
                    RiskApplyResult::Rejected {
                        reason,
                        outcome_tag,
                    } => {
                        risk_outcome = Some(outcome_tag);
                        summary.tally_risk_reject(&reason);
                    }
                },
            }
        }

        let record = record_from(
            &tracked.market,
            &tracked.threshold,
            &pricing,
            &decision,
            cfg.execution.mode,
            risk_outcome,
            execution_outcome,
            now,
        );
        if let Err(e) = logger.record(&record).await {
            warn!(error = %e, ticker = %tracked.market.ticker, "decision log write failed");
        }

        log_decision(
            &tracked.market.ticker,
            &pricing,
            &decision,
            risk_outcome,
            execution_outcome,
        );
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
    /// Trade decision suppressed because the same intended order is already
    /// in-flight in local state.
    suppressed_duplicate_intended: usize,
    /// Execution statuses for Trade rows. Mutually exclusive — each
    /// post-risk Trade increments exactly one of these. (Trades suppressed
    /// before risk by the duplicate-intent guard are counted in
    /// `suppressed_duplicate_intended` instead.)
    exec_paper_submitted: usize,
    exec_paper_suppressed_kill_switch: usize,
    exec_paper_suppressed_no_client: usize,
    exec_dry_run_suppressed: usize,
    exec_live_guarded: usize,
    exec_errors: usize,
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
    /// ECMWF IFS-0.25° ensemble σ skipped because the cached run is
    /// stale. Same fallback semantics as `gefs_source_stale` — the
    /// pricing layer pools whichever source is still warm.
    ecmwf_source_stale: usize,
    /// Markets whose pricing was overridden by an intraday METAR lock.
    /// These rows stamp `sigma_source = "metar_lock"` and a near-1.0
    /// model probability — strategy decides whether to actually trade
    /// based on the market price.
    intraday_lock_hits: usize,
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

    fn tally_execution(&mut self, execution_outcome: &str) {
        match execution_outcome {
            "paper_submitted" => self.exec_paper_submitted += 1,
            "paper_suppressed_never_send" => self.exec_paper_suppressed_kill_switch += 1,
            "paper_suppressed_no_client" => self.exec_paper_suppressed_no_client += 1,
            "dry_run_suppressed" => self.exec_dry_run_suppressed += 1,
            "live_guarded" => self.exec_live_guarded += 1,
            "paper_error" => self.exec_errors += 1,
            _ => {}
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
            suppressed_duplicate_intended = self.suppressed_duplicate_intended,
            exec_paper_submitted = self.exec_paper_submitted,
            exec_paper_suppressed_kill_switch = self.exec_paper_suppressed_kill_switch,
            exec_paper_suppressed_no_client = self.exec_paper_suppressed_no_client,
            exec_dry_run_suppressed = self.exec_dry_run_suppressed,
            exec_live_guarded = self.exec_live_guarded,
            exec_errors = self.exec_errors,
            nws_source_stale = self.nws_source_stale,
            gefs_source_stale = self.gefs_source_stale,
            ecmwf_source_stale = self.ecmwf_source_stale,
            intraday_lock_hits = self.intraday_lock_hits,
            "strategy pass complete"
        );
    }
}

/// Refresh the cached ensemble for `(source, city)` if it's missing or
/// stale. Failures are logged at warn but do not propagate — the strategy
/// loop falls back to whichever sources are still fresh, or to the static
/// σ table if neither is.
///
/// Returns `true` when the cache crossed the staleness threshold AND the
/// most recent refresh attempt failed. The caller treats this as
/// "this source is unavailable" and bumps the per-source stale counter.
async fn ensure_ensemble_fresh(
    gefs: &GefsClient,
    cache: &EnsembleCache,
    city: &CitySpec,
    refresh_after: Duration,
    source: EnsembleSource,
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
    let fetch = match source {
        EnsembleSource::Gefs => gefs.fetch_gfs05(city.lat, city.lon, 7).await,
        EnsembleSource::Ecmwf => gefs.fetch_ecmwf_ifs025(city.lat, city.lon, 7).await,
    };
    match fetch {
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
                    source = source.label(),
                    city = city.kalshi_code,
                    error = %e,
                    age_secs = age.as_secs(),
                    threshold_secs = threshold.as_secs(),
                    "source_stale: ensemble cache past staleness threshold and refresh failed; falling back to other sources / static σ"
                );
                true
            } else {
                warn!(source = source.label(), city = city.kalshi_code, error = %e, "ensemble fetch failed; falling back to other sources / static σ");
                false
            }
        }
    }
}

/// Is `now` inside the standard-time settlement window for this market?
/// True iff the threshold's `date` is "today" in the city's standard
/// time AND we haven't yet crossed the window-end. The CLI report we
/// settle against can only reference observations that already happened,
/// so a future-dated market has nothing to lock against.
fn is_settlement_today(city: &CitySpec, threshold: &weather_types::WeatherThreshold) -> bool {
    let (start, end) = match threshold.stat {
        TempStat::DailyHigh => daily_high_window_utc(city, threshold.date),
        TempStat::DailyLow => daily_low_window_utc(city, threshold.date),
    };
    let now = Utc::now();
    now >= start && now < end
}

/// Refresh the cached METAR observations for `city` if the fetch is
/// missing or older than `refresh_after`. Failures are logged at warn
/// but don't propagate — the lock path falls through to ensemble
/// pricing whenever a fresh snapshot isn't available.
///
/// Pulls a 24h window ending now; that's enough to cover the
/// standard-time settlement window for any city (the longest is 24h)
/// without paginating. NWS doesn't document a rate limit; one request
/// per city per (>=5min) cache miss is well inside "reasonable use".
async fn ensure_metar_fresh(
    metar: &MetarClient,
    cache: &MetarCache,
    city: &CitySpec,
    refresh_after: Duration,
) {
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
        return;
    }
    let end = Utc::now();
    let start = end - chrono::Duration::hours(24);
    match metar.fetch_observations(city.icao, start, end).await {
        Ok(obs) => {
            let mut write = cache.write().await;
            write.insert(city.kalshi_code.to_string(), (obs, Utc::now()));
        }
        Err(e) => {
            warn!(city = city.kalshi_code, station = city.icao, error = %e,
                "METAR fetch failed; intraday lock unavailable for this market");
        }
    }
}

/// Pull whichever ensemble caches are populated and warm enough to use,
/// pool their per-member daily extremes, and derive `(σ, source_tag)` for
/// this market's (date, stat). Returns `None` whenever no source produced
/// a usable in-window σ — the caller falls back to the static σ table.
///
/// The `*_enabled` flags also gate cache *reads*, so a flipped-off source
/// is ignored even if a stale fetch left data behind.
async fn resolve_ensemble_sigma(
    gefs_cache: &EnsembleCache,
    ecmwf_cache: &EnsembleCache,
    city: &CitySpec,
    threshold: &weather_types::WeatherThreshold,
    gefs_enabled: bool,
    ecmwf_enabled: bool,
) -> Option<(f64, &'static str)> {
    let gefs_read = gefs_cache.read().await;
    let ecmwf_read = ecmwf_cache.read().await;
    let gefs_forecast = if gefs_enabled {
        gefs_read.get(city.kalshi_code).map(|(f, _)| f)
    } else {
        None
    };
    let ecmwf_forecast = if ecmwf_enabled {
        ecmwf_read.get(city.kalshi_code).map(|(f, _)| f)
    } else {
        None
    };
    let mut sources: Vec<&EnsembleForecast> = Vec::with_capacity(2);
    if let Some(f) = gefs_forecast {
        sources.push(f);
    }
    if let Some(f) = ecmwf_forecast {
        sources.push(f);
    }
    if sources.is_empty() {
        return None;
    }
    let stat = match threshold.stat {
        TempStat::DailyHigh => pooled_daily_high_stats(&sources, city, threshold.date)?,
        TempStat::DailyLow => pooled_daily_low_stats(&sources, city, threshold.date)?,
    };
    // A handful of members can produce a zero or near-zero σ that's just
    // sample noise. Guard against that by ignoring tiny σ — we'd rather
    // fall back to the static table than over-confidently price near 0/1.
    if stat.n_members < 5 || stat.sigma_f < 0.25 {
        return None;
    }
    let tag = match (gefs_forecast.is_some(), ecmwf_forecast.is_some()) {
        (true, true) => SIGMA_SOURCE_GEFS_ECMWF_BLEND,
        (true, false) => SIGMA_SOURCE_GEFS_ENSEMBLE,
        (false, true) => SIGMA_SOURCE_ECMWF_ENSEMBLE,
        (false, false) => unreachable!("sources non-empty checked above"),
    };
    Some((stat.sigma_f, tag))
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
enum RiskApplyResult {
    Approved {
        signal: weather_types::Signal,
        outcome_tag: &'static str,
    },
    Rejected {
        reason: RejectReason,
        outcome_tag: &'static str,
    },
}

fn apply_risk(risk: &mut RiskManager, signal: &weather_types::Signal) -> RiskApplyResult {
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
            RiskApplyResult::Approved {
                signal: s,
                outcome_tag: "approved",
            }
        }
        RiskDecision::Adjusted(s, reason) => {
            let outcome_tag = match reason {
                weather_risk::AdjustReason::PositionSizeClipped => "adjusted_position_size_clipped",
                weather_risk::AdjustReason::TotalExposureClipped => {
                    "adjusted_total_exposure_clipped"
                }
            };
            info!(
                ticker = %s.market_ticker,
                contracts = s.contracts,
                original_contracts,
                limit_price = %s.limit_price,
                reason = ?reason,
                pass_exposure_usd = %risk.pass_exposure_usd(),
                "risk: clipped position size"
            );
            RiskApplyResult::Approved {
                signal: s,
                outcome_tag,
            }
        }
        RiskDecision::Reject(reason) => {
            let outcome_tag = match reason {
                RejectReason::InCooldown { .. } => "rejected_in_cooldown",
                RejectReason::ConcurrentPositionsCapped => "rejected_concurrent_capped",
                RejectReason::NoBudgetRemaining => "rejected_no_budget",
            };
            info!(
                ticker = %signal.market_ticker,
                reason = ?reason,
                pass_exposure_usd = %risk.pass_exposure_usd(),
                pass_position_count = risk.pass_position_count(),
                "risk: rejected"
            );
            RiskApplyResult::Rejected {
                reason,
                outcome_tag,
            }
        }
    }
}

fn log_decision(
    ticker: &str,
    pricing: &weather_pricing::ModelPricing,
    decision: &Decision,
    risk_outcome: Option<&str>,
    execution_outcome: Option<&str>,
) {
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
                risk_outcome = ?risk_outcome,
                execution_outcome = ?execution_outcome,
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

impl TradeIntentKey {
    fn from_signal(signal: &weather_types::Signal) -> Self {
        Self {
            ticker: signal.market_ticker.clone(),
            side: format!("{:?}", signal.side),
            limit_price: signal.limit_price.normalize().to_string(),
            contracts: signal.contracts,
        }
    }
}

fn prune_intended_trades(intended_trades: &mut IntendedTrades) {
    let cutoff = Utc::now() - chrono::Duration::hours(INTENDED_TRADE_TTL_HOURS);
    intended_trades.retain(|_, ts| *ts > cutoff);
}

fn should_register_intended_trade(execution_outcome: &str) -> bool {
    matches!(
        execution_outcome,
        "dry_run_suppressed" | "paper_suppressed_never_send" | "paper_submitted"
    )
}

async fn execute_trade(
    mode: ExecutionMode,
    order_client: Option<&mut KalshiOrderClient>,
    signal: &weather_types::Signal,
) -> &'static str {
    match mode {
        ExecutionMode::DryRun => "dry_run_suppressed",
        ExecutionMode::Live => "live_guarded",
        ExecutionMode::Paper => {
            let Some(client) = order_client else {
                warn!(
                    ticker = %signal.market_ticker,
                    "paper mode: no executor client available; suppressing send"
                );
                return "paper_suppressed_no_client";
            };
            let req = OrderRequest::from_signal(signal);
            match client.place_order(&req).await {
                Ok(Some(_)) => "paper_submitted",
                Ok(None) => "paper_suppressed_never_send",
                Err(e) => {
                    warn!(
                        ticker = %signal.market_ticker,
                        error = %e,
                        "paper mode order placement failed"
                    );
                    "paper_error"
                }
            }
        }
    }
}

fn build_order_client(cfg: &AppConfig) -> Option<KalshiOrderClient> {
    if !matches!(
        cfg.execution.mode,
        ExecutionMode::Paper | ExecutionMode::Live
    ) {
        return None;
    }
    let key_id = match AppConfig::kalshi_api_key_id() {
        Ok(v) => v,
        Err(e) => {
            warn!(error = %e, "executor disabled: missing KALSHI_API_KEY_ID");
            return None;
        }
    };
    let key_path = match AppConfig::kalshi_private_key_path() {
        Ok(v) => v,
        Err(e) => {
            warn!(error = %e, "executor disabled: missing KALSHI_PRIVATE_KEY_PATH");
            return None;
        }
    };
    let signer = match KalshiSigner::from_pem_file(std::path::Path::new(&key_path)) {
        Ok(s) => s,
        Err(e) => {
            warn!(error = %e, path = %key_path, "executor disabled: invalid private key");
            return None;
        }
    };
    Some(KalshiOrderClient::new(
        cfg.kalshi.base_url().to_string(),
        key_id,
        signer,
    ))
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
