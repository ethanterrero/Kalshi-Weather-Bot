use anyhow::Result;
use tracing::info;
use weather_config::AppConfig;

#[tokio::main]
async fn main() -> Result<()> {
    let config = AppConfig::load()?;
    init_tracing(&config.logging);

    info!(
        kalshi_env = %config.kalshi.env,
        kalshi_base = %config.kalshi.base_url(),
        nws_base = %config.forecast.nws_base_url,
        execution = ?config.execution.mode,
        "Kalshi Weather Bot starting"
    );

    // TODO: wire scanner → monitor → forecast → pricing → strategy → risk → executor.
    info!("v0 scaffold — main loop not yet implemented; exiting cleanly");
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
