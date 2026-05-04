mod decision_log;

use anyhow::Result;
use chrono::{DateTime, Utc};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tracing::{info, warn};
use weather_config::{AppConfig, ExecutionMode};
use weather_forecast::NwsClient;
use weather_pricing::{price_market, PricingError};
use weather_risk::{RiskDecision, RiskManager};
use weather_scanner::MarketScanner;
use weather_strategy::{decide, Decision, FeeModel, NoTradeReason};
use weather_types::{lookup_city, Forecast};

use decision_log::{record_from, DecisionLogger};

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
    let logger_for_strat = decision_logger.clone();
    let mut risk_for_strat = RiskManager::new(config.risk.clone());
    info!(
        max_position_size_usd = %config.risk.max_position_size_usd,
        max_total_exposure_usd = %config.risk.max_total_exposure_usd,
        max_concurrent_positions = config.risk.max_concurrent_positions,
        "risk caps active"
    );
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
                &logger_for_strat,
                &mut risk_for_strat,
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

    // Each pass starts with a clean risk tally — dry-run has no fill
    // confirmations to persist between passes. When live trading lands this
    // gets replaced with a snapshot pulled from /portfolio/positions.
    risk.reset_for_pass();

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
                Some((_, ts)) => {
                    Utc::now()
                        .signed_duration_since(*ts)
                        .to_std()
                        .unwrap_or_default()
                        > refresh_after
                }
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

        // NWS-update lockout: if the forecast we're pricing against was
        // issued in the last `nws_lockout_after_update_secs`, sit out the
        // trade — arbitrage bots reprice within seconds of an issue and
        // we don't want to be on the wrong side of that. The freshness
        // clock is NWS's `generatedAt`, not our local fetch time.
        let decision =
            match nws_lockout_decision(&forecast, cfg.forecast.nws_lockout_after_update_secs) {
                Some(reason) => Decision::NoTrade(reason),
                None => decide(&tracked.market, &pricing, &cfg.strategy, &fees),
            };

        let record = record_from(
            &tracked.market,
            &tracked.threshold,
            &pricing,
            &decision,
            Utc::now(),
        );
        if let Err(e) = logger.record(&record).await {
            warn!(error = %e, ticker = %tracked.market.ticker, "decision log write failed");
        }

        log_decision(&tracked.market.ticker, &pricing, &decision);

        // Run Trade signals through the risk layer. Position-size and
        // total-exposure caps may clip the contract count; the concurrent-
        // positions cap may reject outright.
        if let Decision::Trade(sig, _) = &decision {
            apply_risk(risk, sig);
        }
    }
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

fn apply_risk(risk: &mut RiskManager, signal: &weather_types::Signal) {
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
        }
        RiskDecision::Reject(reason) => {
            info!(
                ticker = %signal.market_ticker,
                reason = ?reason,
                pass_exposure_usd = %risk.pass_exposure_usd(),
                pass_position_count = risk.pass_position_count(),
                "risk: rejected"
            );
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
}
