use anyhow::Result;
use std::sync::Arc;
use std::time::Duration;
use tracing::{info, warn};
use weather_config::AppConfig;
use weather_scanner::MarketScanner;

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

    let scanner = Arc::new(MarketScanner::new(&config));

    // Initial scan so the bot has a market list before any other work runs.
    info!("Running initial Kalshi market scan...");
    match scanner.refresh().await {
        Ok(count) => info!(count, "Initial scan complete"),
        Err(e) => warn!(error = %e, "Initial scan failed; continuing with empty market set"),
    }

    // Periodic refresh — for v1 the scanner is also our price feed (each scan
    // re-reads yes_bid/yes_ask/last_price from Kalshi's /markets), so we run
    // this on the `monitor.poll_interval_ms` cadence rather than the slower
    // `scanner.refresh_interval_secs`. WebSocket-based price updates are a
    // future improvement.
    let scanner_for_loop = scanner.clone();
    let poll_ms = config.monitor.poll_interval_ms.max(5_000);
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_millis(poll_ms));
        interval.tick().await; // skip the immediate fire — initial scan already ran.
        loop {
            interval.tick().await;
            if let Err(e) = scanner_for_loop.refresh().await {
                warn!(error = %e, "Periodic market scan failed");
            }
        }
    });

    // TODO: forecast → pricing → strategy → risk → executor wiring.
    info!("v0 scaffold — strategy loop not yet implemented; idling on scan refresh task");

    // Keep the process alive so the background scan loop runs. ctrl+C exits.
    tokio::signal::ctrl_c().await?;
    info!("Shutting down");
    Ok(())
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
