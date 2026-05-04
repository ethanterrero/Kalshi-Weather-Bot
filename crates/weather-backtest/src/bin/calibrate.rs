//! `calibrate` — Phase A model-calibration runner.
//!
//! Pulls archived deterministic GFS forecasts (Open-Meteo) for a (city,
//! date range), pulls IEM CLI for the same range, walks the requested
//! strike grid, and prints calibration metrics. Use this *before* the
//! 2-week dry-run to verify the model's `P(YES)` is well-calibrated
//! against ground truth on past data.
//!
//! Example:
//!
//!   cargo run -p weather-backtest --bin calibrate -- \
//!       --city NY --start 2025-04-01 --end 2025-09-30 \
//!       --high-strikes 60,65,70,75,80,85,90,95
//!
//! σ defaults to `weather_pricing::sigma_for_horizon(0) = 1.6` (the
//! hand-calibrated day-0 NWS forecast error). Override with `--sigma 2.4`
//! to see how the metrics shift if you assume different forecast spread.
//! v2 of this binary will plug GEFS-derived σ in here automatically.

use anyhow::{anyhow, Context, Result};
use chrono::NaiveDate;
use clap::Parser;
use std::collections::HashSet;

use weather_backtest::{historical_calibration, metrics, CalibrationPlan, JoinedDecision};
use weather_forecast::{HistoricalForecastClient, IemCliClient};
use weather_pricing::sigma_for_horizon;
use weather_types::lookup_city;

#[derive(Debug, Parser)]
#[command(
    name = "calibrate",
    about = "Phase A model-calibration runner: archived GFS vs IEM CLI"
)]
struct Args {
    /// Kalshi city code (e.g. NY, CHI, LAX, MIA, AUS).
    #[arg(long)]
    city: String,
    /// Start date (inclusive), YYYY-MM-DD.
    #[arg(long)]
    start: NaiveDate,
    /// End date (inclusive), YYYY-MM-DD.
    #[arg(long)]
    end: NaiveDate,
    /// Comma-separated high-temperature strikes (°F integer). E.g.
    /// `60,65,70,75,80,85,90`.
    #[arg(long, value_delimiter = ',')]
    high_strikes: Vec<i32>,
    /// Comma-separated low-temperature strikes (°F integer).
    #[arg(long, value_delimiter = ',')]
    low_strikes: Vec<i32>,
    /// σ (°F) to feed the Normal-CDF. Defaults to the bot's configured
    /// day-0 horizon σ (1.6 today).
    #[arg(long)]
    sigma: Option<f64>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    if args.start > args.end {
        return Err(anyhow!(
            "--start ({}) must be <= --end ({})",
            args.start,
            args.end
        ));
    }
    if args.high_strikes.is_empty() && args.low_strikes.is_empty() {
        return Err(anyhow!(
            "supply --high-strikes and/or --low-strikes (got neither)"
        ));
    }

    let city = lookup_city(&args.city).ok_or_else(|| {
        anyhow!(
            "unknown city code '{}'; check crates/weather-types/src/cities.rs",
            args.city
        )
    })?;
    let sigma = args.sigma.unwrap_or_else(|| sigma_for_horizon(0));

    println!(
        "calibrating {} ({}) {}..{} | σ={:.2}°F | strikes: high={:?} low={:?}",
        city.name,
        city.kalshi_code,
        args.start,
        args.end,
        sigma,
        args.high_strikes,
        args.low_strikes
    );

    // 1. Fetch archived forecast for the full range. Open-Meteo accepts
    //    multi-month ranges in a single call up to ~6 months; for longer
    //    stretches we'd need to batch, but that's out of scope here.
    println!("fetching archived GFS forecast from Open-Meteo...");
    let forecast = HistoricalForecastClient::open_meteo()
        .fetch_range(city.lat, city.lon, args.start, args.end)
        .await
        .context("Open-Meteo historical-forecast fetch failed")?;
    println!(
        "  → {} hourly samples ({} to {})",
        forecast.times.len(),
        forecast
            .times
            .first()
            .map(|t| t.to_rfc3339())
            .unwrap_or_default(),
        forecast
            .times
            .last()
            .map(|t| t.to_rfc3339())
            .unwrap_or_default(),
    );

    // 2. Fetch CLI for the matching station, walking each calendar month
    //    the window touches (IEM defaults to 2019 unless year/month
    //    pinned — `fetch_recent` covers `today - days`; for arbitrary
    //    historical ranges we walk months explicitly).
    println!("fetching IEM CLI for {}...", city.icao);
    let cli = fetch_cli_for_range(city.icao, args.start, args.end)
        .await
        .context("IEM CLI fetch failed")?;
    println!("  → {} CLI rows", cli.len());

    // 3. Build the calibration plan + walk the grid.
    let plan = CalibrationPlan {
        dates: dates_inclusive(args.start, args.end),
        high_strikes_f: args.high_strikes,
        low_strikes_f: args.low_strikes,
        sigma_f: sigma,
    };
    let joined = historical_calibration(city, &forecast, &cli, &plan);
    println!("synthesised {} decisions", joined.len());
    if joined.is_empty() {
        return Err(anyhow!(
            "no decisions produced — check that strikes overlap forecasts and CLI covers the range"
        ));
    }

    // 4. Metrics + summary print.
    let m = metrics(&joined);
    print_summary(&m, &joined);

    Ok(())
}

/// Walk each calendar month the `[start, end]` window touches, fetch
/// IEM CLI for that month, and concat. Filtered to the date range.
async fn fetch_cli_for_range(
    station_icao: &str,
    start: NaiveDate,
    end: NaiveDate,
) -> Result<Vec<weather_forecast::CliReport>> {
    use chrono::Datelike;
    let client = IemCliClient::iem_default();
    let mut out = Vec::new();
    let (mut y, mut m) = (start.year(), start.month());
    let end_y = end.year();
    let end_m = end.month();
    loop {
        let chunk = client.fetch_month(station_icao, y, m).await?;
        for r in chunk {
            if r.date >= start && r.date <= end {
                out.push(r);
            }
        }
        if (y, m) == (end_y, end_m) {
            break;
        }
        m += 1;
        if m > 12 {
            m = 1;
            y += 1;
        }
    }
    Ok(out)
}

fn dates_inclusive(start: NaiveDate, end: NaiveDate) -> Vec<NaiveDate> {
    let mut out = Vec::new();
    let mut d = start;
    while d <= end {
        out.push(d);
        d = d
            .succ_opt()
            .expect("succ should never overflow in this date range");
    }
    out
}

fn print_summary(m: &weather_backtest::Metrics, joined: &[JoinedDecision]) {
    println!();
    println!("=== Phase A calibration summary ===");
    println!("  n decisions:        {}", m.n);

    // Cardinality breakdown by stat.
    let n_high = joined
        .iter()
        .filter(|j| j.row.stat == weather_types::TempStat::DailyHigh)
        .count();
    let n_low = m.n - n_high;
    println!("    high markets:     {}", n_high);
    println!("    low markets:      {}", n_low);

    // Distinct dates touched (to gauge sample independence).
    let dates: HashSet<_> = joined.iter().map(|j| j.row.resolution_date).collect();
    println!("    distinct dates:   {}", dates.len());

    println!();
    println!("  hit rate:           {:.4}", m.hit_rate);
    println!(
        "  mean Brier:         {:.4}  (lower is better; 0.25 = chance)",
        m.mean_brier
    );
    println!(
        "  mean log loss:      {:.4}  (lower is better)",
        m.mean_log_loss
    );
    println!(
        "  mean temp bias °F:  {:+.3}  (positive = forecast > realised)",
        m.mean_temp_bias_f
    );

    // Calibration table: predicted bucket vs realised rate.
    println!();
    println!("  calibration (predicted P(YES) bucket → realised YES rate):");
    println!("    bucket   n      mean_pred   realised   diff");
    for (i, b) in m.calibration.iter().enumerate() {
        if b.n == 0 {
            continue;
        }
        let lo = (i as f64) * 0.1;
        let hi = ((i + 1) as f64) * 0.1;
        let diff = b.realised_yes_rate - b.mean_predicted_p;
        println!(
            "    [{:.1}–{:.1})   {:>5}   {:.4}      {:.4}    {:+.4}",
            lo, hi, b.n, b.mean_predicted_p, b.realised_yes_rate, diff
        );
    }
    println!();
    println!("Phase A acceptance criteria reminder (Perplexity refinement):");
    println!("  (b) calibration error ≤5% per bucket");
    println!("  (c) ≥1000 decisions");
    println!("  (d) hit rate ≥0.60, Brier ≤0.20, log-loss ≤0.50");
    println!("  (a) net-of-fee expectancy >0  ← deferred (needs historical Kalshi prices)");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_inclusive_handles_single_day() {
        let d = NaiveDate::from_ymd_opt(2026, 5, 4).unwrap();
        assert_eq!(dates_inclusive(d, d), vec![d]);
    }

    #[test]
    fn dates_inclusive_walks_a_week() {
        let s = NaiveDate::from_ymd_opt(2026, 5, 1).unwrap();
        let e = NaiveDate::from_ymd_opt(2026, 5, 7).unwrap();
        let out = dates_inclusive(s, e);
        assert_eq!(out.len(), 7);
        assert_eq!(out[0], s);
        assert_eq!(out[6], e);
    }

    #[test]
    fn dates_inclusive_crosses_month_boundary() {
        let s = NaiveDate::from_ymd_opt(2026, 1, 30).unwrap();
        let e = NaiveDate::from_ymd_opt(2026, 2, 2).unwrap();
        assert_eq!(dates_inclusive(s, e).len(), 4);
    }
}
