use anyhow::Result;
use chrono::{DateTime, Utc};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tracing::{info, warn};
use weather_config::{AppConfig, ExecutionMode};
use weather_forecast::NwsClient;
use weather_pricing::{price_market, PricingError};
use weather_scanner::MarketScanner;
use weather_strategy::{decide, Decision, FeeModel, NoTradeReason};
use weather_types::{lookup_city, Forecast};

/// Cached forecast keyed on Kalshi city code (e.g. "NY"). Refreshed on its
/// own cadence; the strategy loop reads from this cache rather than calling
/// NWS per market (which would burn the rate limit and be redundant — every
/// market for a city/day shares one forecast).
type ForecastCache = Arc<RwLock<HashMap<String, (Forecast, DateTime<Utc>)>>>;

#[tokio::main]
async fn main() -> Result<()> {
    let config = AppConfig::load()?;
    init_tracing(&config.logging);

    info!(
        kalshi_env = %config.kalshi.env,
        kalshi_base = %config.kalshi.base_url(),
        nws_base = %config.forecast.nws_base_url,
        execution = ?config.execution.mode,
        series_count = config.scanner.series_tickers.len(),
        "Kalshi Weather Bot starting"
    );

    if matches!(config.execution.mode, ExecutionMode::Live) {
        warn!("execution.mode = live; live order placement is NOT implemented yet — staying in dry-run");
    }

    let scanner = Arc::new(MarketScanner::new(&config));
    let nws = Arc::new(NwsClient::new(
        config.forecast.nws_base_url.clone(),
        config.forecast.user_agent.clone(),
    ));
    let forecast_cache: ForecastCache = Arc::new(RwLock::new(HashMap::new()));

    info!("Running initial Kalshi market scan...");
    match scanner.refresh().await {
        Ok(count) => info!(count, "Initial scan complete"),
        Err(e) => warn!(error = %e, "Initial scan failed; continuing with empty market set"),
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
    let cache_for_strat = forecast_cache.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_millis(poll_ms));
        interval.tick().await;
        loop {
            interval.tick().await;
            run_strategy_pass(
                &scanner_for_strat,
                &nws_for_strat,
                &cache_for_strat,
                &cfg_for_strat,
            )
            .await;
        }
    });

    info!("Strategy loop running in dry-run; ctrl+C to exit");

    tokio::signal::ctrl_c().await?;
    info!("Shutting down");
    Ok(())
}

/// Read tracked markets, refresh forecasts on demand, run pricing + EV gate
/// per market, and log the decision.
async fn run_strategy_pass(
    scanner: &MarketScanner,
    nws: &NwsClient,
    cache: &ForecastCache,
    cfg: &AppConfig,
) {
    let snapshot = {
        let lock = scanner.markets();
        let guard = lock.read().await;
        guard.clone()
    };
    if snapshot.is_empty() {
        return;
    }

    let fees = FeeModel {
        multiplier: cfg.strategy.fee_multiplier,
    };
    let refresh_after = Duration::from_secs(cfg.forecast.refresh_interval_secs);

    for tracked in snapshot {
        let city_code = &tracked.threshold.city;
        let Some(city) = lookup_city(city_code) else {
            warn!(city = city_code, ticker = %tracked.market.ticker,
                "skipping: city not in mapping table; cannot validate NWS settlement station");
            continue;
        };

        // Refresh-on-demand: if we don't have a forecast for this city or it's
        // older than refresh_interval_secs, fetch a fresh one. NWS rate-limits
        // shared User-Agents so we keep it serial here.
        let needs_refresh = {
            let read = cache.read().await;
            match read.get(city_code) {
                None => true,
                Some((_, ts)) => Utc::now().signed_duration_since(*ts).to_std().unwrap_or_default()
                    > refresh_after,
            }
        };
        if needs_refresh {
            match nws.fetch_point_forecast(city.lat, city.lon).await {
                Ok(f) => {
                    let mut write = cache.write().await;
                    write.insert(city_code.clone(), (f, Utc::now()));
                }
                Err(e) => {
                    warn!(city = city_code, error = %e, "NWS forecast fetch failed");
                    continue;
                }
            }
        }

        let forecast = {
            let read = cache.read().await;
            match read.get(city_code) {
                Some((f, _)) => f.clone(),
                None => continue,
            }
        };

        let pricing = match price_market(&tracked.threshold, &forecast) {
            Ok(p) => p,
            Err(PricingError::NoMatchingForecastPeriod) => {
                // Common case: market is more than 7 days out (NWS horizon).
                continue;
            }
            Err(e) => {
                warn!(ticker = %tracked.market.ticker, error = %e, "pricing failed");
                continue;
            }
        };

        let decision = decide(&tracked.market, &pricing, &cfg.strategy, &fees);
        log_decision(&tracked.market.ticker, &pricing, &decision);
    }
}

fn log_decision(
    ticker: &str,
    pricing: &weather_pricing::ModelPricing,
    decision: &Decision,
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
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(&config.level));
    if config.json_output {
        fmt().with_env_filter(filter).json().init();
    } else {
        fmt().with_env_filter(filter).with_target(true).init();
    }
}
