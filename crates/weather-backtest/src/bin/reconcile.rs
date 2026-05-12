//! `reconcile` — settlement reconciliation between Kalshi and IEM CLI.
//!
//! Pull the last N days of settled KXHIGH / KXLOW markets from Kalshi,
//! join each to the IEM CLI report for its (station, date), and check
//! that our locally-computed YES outcome agrees with Kalshi's `result`
//! field. Any drift = a station-table or ticker-parser bug, and silently
//! invalidates every metric the replay binary prints.
//!
//! Default series list is the bot's v1 hard-coded set: NY/CHI/LAX/MIA/AUS
//! × HIGH/LOW = 10 series. Override with `--series KXHIGHNY,KXLOWLAX`.
//!
//! Example:
//!
//!   cargo run -p weather-backtest --bin reconcile -- --lookback-days 30

use anyhow::{Context, Result};
use clap::Parser;
use std::collections::BTreeMap;

use weather_backtest::{
    reconcile_settled_market, KalshiOutcome, ReconcileOutcome, ReconcileResult, SkipReason,
};
use weather_forecast::IemCliClient;
use weather_scanner::KalshiClient;
use weather_types::lookup_city;

const DEFAULT_SERIES: &[&str] = &[
    "KXHIGHNY",
    "KXLOWNY",
    "KXHIGHCHI",
    "KXLOWCHI",
    "KXHIGHLAX",
    "KXLOWLAX",
    "KXHIGHMIA",
    "KXLOWMIA",
    "KXHIGHAUS",
    "KXLOWAUS",
];

#[derive(Debug, Parser)]
#[command(
    name = "reconcile",
    about = "Reconcile settled Kalshi KXHIGH/KXLOW markets against IEM CLI"
)]
struct Args {
    /// Series tickers to reconcile. Defaults to the bot's 10 v1 series.
    #[arg(long, value_delimiter = ',')]
    series: Vec<String>,
    /// Lookback window for settled markets (days).
    #[arg(long, default_value_t = 30)]
    lookback_days: i64,
    /// Kalshi API base URL ending in `/trade-api/v2`.
    #[arg(long, default_value = "https://demo-api.kalshi.co/trade-api/v2")]
    kalshi_base_url: String,
    /// Print every reconciled row, not just drifts and the summary.
    #[arg(long)]
    verbose: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let series: Vec<String> = if args.series.is_empty() {
        DEFAULT_SERIES.iter().map(|s| s.to_string()).collect()
    } else {
        args.series
    };

    let kalshi = KalshiClient::new(args.kalshi_base_url.clone());
    let cli_client = IemCliClient::iem_default();

    // Pre-flight every series so a single failure isn't fatal.
    println!(
        "fetching settled markets for {} series, last {} days from {}",
        series.len(),
        args.lookback_days,
        args.kalshi_base_url
    );

    let mut all_settled = Vec::new();
    for s in &series {
        match kalshi
            .list_settled_markets_in_series(s, args.lookback_days)
            .await
        {
            Ok(v) => {
                println!("  {s}: {} settled", v.len());
                all_settled.extend(v);
            }
            Err(e) => eprintln!("  {s}: fetch failed: {e}"),
        }
    }

    if all_settled.is_empty() {
        println!("no settled markets returned; nothing to reconcile");
        return Ok(());
    }

    // CLI per (station, lookback window). Fetch once per station instead of
    // per market — IEM serves monthly chunks, so this is N stations × M
    // months rather than thousands of point lookups.
    let mut stations: Vec<String> = Vec::new();
    for raw in &all_settled {
        if let Some(threshold) =
            weather_scanner::parse_weather_threshold(&raw.ticker, &raw.strike_type)
        {
            if let Some(city) = lookup_city(&threshold.city) {
                if !stations.iter().any(|s| s == city.icao) {
                    stations.push(city.icao.to_string());
                }
            }
        }
    }

    let cli_lookback_days = (args.lookback_days + 7).max(31);
    let mut cli_rows = Vec::new();
    println!();
    println!(
        "fetching IEM CLI for {} stations ({} days)",
        stations.len(),
        cli_lookback_days
    );
    for station in &stations {
        let chunk = cli_client
            .fetch_recent(station, cli_lookback_days as u32)
            .await
            .with_context(|| format!("IEM CLI fetch failed for {station}"))?;
        println!("  {station}: {} CLI reports", chunk.len());
        cli_rows.extend(chunk);
    }

    let mut reconciled: Vec<ReconcileResult> = Vec::new();
    let mut skipped: Vec<(String, SkipReason)> = Vec::new();
    for raw in &all_settled {
        match reconcile_settled_market(raw, &cli_rows) {
            ReconcileOutcome::Reconciled(r) => reconciled.push(r),
            ReconcileOutcome::Skipped { ticker, reason } => skipped.push((ticker, reason)),
        }
    }

    print_summary(&reconciled, &skipped);
    print_drifts(&reconciled);
    if args.verbose {
        print_all(&reconciled);
    }
    print_skips(&skipped);

    // Non-zero exit code on any drift makes this trivially CI-able later.
    if reconciled.iter().any(|r| !r.agrees) {
        std::process::exit(2);
    }
    Ok(())
}

fn print_summary(reconciled: &[ReconcileResult], skipped: &[(String, SkipReason)]) {
    let drift = reconciled.iter().filter(|r| !r.agrees).count();
    let agree = reconciled.len() - drift;
    println!();
    println!("=== Reconciliation summary ===");
    println!(
        "  reconciled: {} ({} agree, {} drift)",
        reconciled.len(),
        agree,
        drift
    );
    println!("  skipped:    {}", skipped.len());

    // By city / station — easier to spot single-station bugs.
    let mut by_station: BTreeMap<&str, (usize, usize)> = BTreeMap::new();
    for r in reconciled {
        let entry = by_station.entry(r.station.as_str()).or_insert((0, 0));
        entry.0 += 1;
        if !r.agrees {
            entry.1 += 1;
        }
    }
    for (station, (n, n_drift)) in by_station {
        println!("    {station}: {n} reconciled, {n_drift} drift");
    }
}

fn print_drifts(reconciled: &[ReconcileResult]) {
    let drifts: Vec<&ReconcileResult> = reconciled.iter().filter(|r| !r.agrees).collect();
    if drifts.is_empty() {
        return;
    }
    println!();
    println!("=== Drifts ({} total) ===", drifts.len());
    println!(
        "  {:32} {:5} {:10} dir       strike  realised  kalshi  computed",
        "ticker", "stn", "date"
    );
    for r in drifts {
        println!(
            "  {:32} {:5} {:10} {:9} {:>5}°F  {:>5}°F   {:6}  {:8}",
            r.ticker,
            r.station,
            r.resolution_date,
            format!("{:?}", r.direction).to_lowercase(),
            r.strike_temperature_f,
            r.realised_temp_f,
            match r.kalshi_result {
                KalshiOutcome::Yes => "YES",
                KalshiOutcome::No => "NO",
            },
            if r.computed_yes { "YES" } else { "NO" },
        );
    }
}

fn print_all(reconciled: &[ReconcileResult]) {
    println!();
    println!("=== All reconciled rows ===");
    for r in reconciled {
        println!(
            "  {} {} {} strike={}°F realised={}°F kalshi={:?} computed={} agrees={}",
            r.ticker,
            r.station,
            r.resolution_date,
            r.strike_temperature_f,
            r.realised_temp_f,
            r.kalshi_result,
            r.computed_yes,
            r.agrees
        );
    }
}

fn print_skips(skipped: &[(String, SkipReason)]) {
    if skipped.is_empty() {
        return;
    }
    let mut by_kind: BTreeMap<&'static str, usize> = BTreeMap::new();
    for (_, reason) in skipped {
        let kind = match reason {
            SkipReason::UnsupportedTicker => "unsupported_ticker",
            SkipReason::UnknownCity(_) => "unknown_city",
            SkipReason::MissingOrUnparseableResult(_) => "missing_or_unparseable_result",
            SkipReason::MissingCli { .. } => "missing_cli",
            SkipReason::MissingTempInCli { .. } => "missing_temp_in_cli",
        };
        *by_kind.entry(kind).or_default() += 1;
    }
    println!();
    println!("=== Skips ({} total) ===", skipped.len());
    for (kind, n) in by_kind {
        println!("  {kind}: {n}");
    }
}
