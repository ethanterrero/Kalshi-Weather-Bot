//! `replay` — JSONL dry-run replay with outcome metrics and candle marks.
//!
//! Reads one or more bot decision-log files, optionally joins resolved rows
//! to IEM CLI outcomes, and optionally joins unique Trade opportunities to
//! Kalshi candles for a conservative immediate spread/fee mark. This is
//! intended for the dry run: start the bot, let JSONL accumulate, then use
//! this binary to answer whether GEFS σ rows are behaving better than
//! static-fallback rows.

use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, Datelike, Duration, NaiveDate, Utc};
use clap::{Parser, ValueEnum};
use std::collections::BTreeMap;
use std::path::PathBuf;

use weather_backtest::{
    candle_containing_ts, dedupe_joined_by_opportunity, estimate_trade_pnl,
    filter_joined_to_trades, is_trade_row, join_outcome, metrics_by_sigma_source, read_decisions,
    series_ticker_from_market_ticker, summarize_pnl, summarize_pnl_by_sigma_source, DecisionRow,
    JoinedDecision, PnlEstimate,
};
use weather_forecast::IemCliClient;
use weather_scanner::{KalshiClient, PeriodInterval};

#[derive(Debug, Parser)]
#[command(
    name = "replay",
    about = "Replay weather-bot JSONL decisions; split metrics/P&L by sigma_source"
)]
struct Args {
    /// Decision JSONL file(s) or directories containing `*.jsonl`.
    #[arg(required = true)]
    decisions: Vec<PathBuf>,
    /// Skip IEM CLI outcome fetch. Useful before markets have resolved.
    #[arg(long)]
    skip_outcomes: bool,
    /// Skip Kalshi candle fetch / immediate spread+fee mark.
    #[arg(long)]
    skip_candles: bool,
    /// Kalshi API base URL ending in `/trade-api/v2`.
    #[arg(long, default_value = "https://demo-api.kalshi.co/trade-api/v2")]
    kalshi_base_url: String,
    /// Candle interval used for mark-to-market.
    #[arg(long, value_enum, default_value_t = ReplayPeriod::Hour)]
    period: ReplayPeriod,
    /// Restrict outcome metrics to rows where `decision == "trade"`. The
    /// default view spans the whole priced universe and is dominated by
    /// far-tail no-trade rows where the model correctly prices near 0 or
    /// 1; this flag scopes the histogram to the subset where edge
    /// matters. The candle mark section is unaffected (it already only
    /// covers trade rows).
    #[arg(long)]
    trades_only: bool,
    /// Collapse repeated dry-run emissions to one per
    /// `(ticker, resolution_date)` before computing outcome metrics.
    /// Without this, calibration buckets are dominated by markets the
    /// bot re-emits every pass for hours. Earliest emission wins.
    #[arg(long)]
    by_opportunity: bool,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum ReplayPeriod {
    Minute,
    Hour,
    Day,
}

impl From<ReplayPeriod> for PeriodInterval {
    fn from(value: ReplayPeriod) -> Self {
        match value {
            ReplayPeriod::Minute => PeriodInterval::OneMinute,
            ReplayPeriod::Hour => PeriodInterval::OneHour,
            ReplayPeriod::Day => PeriodInterval::OneDay,
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let paths = expand_decision_paths(&args.decisions)?;
    if paths.is_empty() {
        return Err(anyhow!("no JSONL decision files found"));
    }

    let mut rows = Vec::new();
    for path in &paths {
        let mut chunk =
            read_decisions(path).with_context(|| format!("failed reading {}", path.display()))?;
        rows.append(&mut chunk);
    }
    rows.sort_by_key(|r| r.ts);

    let trade_rows: Vec<_> = rows.iter().filter(|r| is_trade_row(r)).cloned().collect();
    let unique_trade_rows = dedupe_trade_rows(&trade_rows);
    println!(
        "loaded {} decision rows from {} file(s)",
        rows.len(),
        paths.len()
    );
    println!("  raw trade rows: {}", trade_rows.len());
    println!("  unique trade opportunities: {}", unique_trade_rows.len());
    if trade_rows.len() != unique_trade_rows.len() {
        println!(
            "  deduped repeated dry-run emissions: {}",
            trade_rows.len() - unique_trade_rows.len()
        );
    }
    print_row_counts_by_sigma(&rows);

    if !args.skip_outcomes {
        let joined = fetch_and_join_outcomes(&rows).await?;
        println!();
        println!("=== Outcome metrics by sigma_source ===");
        println!("joined {} rows with realised CLI outcomes", joined.len());

        let filters_applied = describe_outcome_filters(args.trades_only, args.by_opportunity);
        let scoped = apply_outcome_filters(&joined, args.trades_only, args.by_opportunity);
        if let Some(desc) = filters_applied {
            println!(
                "  after filters ({desc}): {} rows / opportunities",
                scoped.len()
            );
        }

        if scoped.is_empty() {
            println!("  no resolved rows yet; rerun after CLI reports publish");
        } else {
            for (source, m) in metrics_by_sigma_source(&scoped) {
                print_metrics_line(&source, &m);
            }
        }
    }

    if !args.skip_candles {
        let interval = PeriodInterval::from(args.period);
        let pnl =
            fetch_and_estimate_pnl(&args.kalshi_base_url, &unique_trade_rows, interval).await?;
        println!();
        println!("=== Immediate candle spread/fee mark ===");
        println!(
            "This is not realised strategy P&L. It marks each unique dry-run opportunity to the bid-side liquidation price inside the same candle, so it mostly measures spread + entry fee friction."
        );
        let summary = summarize_pnl(&pnl, unique_trade_rows.len());
        print_pnl_line("all", &summary);
        for (source, s) in summarize_pnl_by_sigma_source(&pnl, &unique_trade_rows) {
            print_pnl_line(&source, &s);
        }
    }

    Ok(())
}

fn apply_outcome_filters(
    joined: &[JoinedDecision],
    trades_only: bool,
    by_opportunity: bool,
) -> Vec<JoinedDecision> {
    let mut out = if trades_only {
        filter_joined_to_trades(joined)
    } else {
        joined.to_vec()
    };
    if by_opportunity {
        out = dedupe_joined_by_opportunity(&out);
    }
    out
}

fn describe_outcome_filters(trades_only: bool, by_opportunity: bool) -> Option<String> {
    let mut parts = Vec::new();
    if trades_only {
        parts.push("--trades-only");
    }
    if by_opportunity {
        parts.push("--by-opportunity");
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(", "))
    }
}

fn expand_decision_paths(inputs: &[PathBuf]) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    for input in inputs {
        if input.is_dir() {
            for entry in std::fs::read_dir(input)
                .with_context(|| format!("failed reading directory {}", input.display()))?
            {
                let path = entry?.path();
                if path.extension().and_then(|s| s.to_str()) == Some("jsonl") {
                    out.push(path);
                }
            }
        } else if input.is_file() {
            out.push(input.clone());
        } else {
            return Err(anyhow!("path does not exist: {}", input.display()));
        }
    }
    out.sort();
    Ok(out)
}

fn print_row_counts_by_sigma(rows: &[DecisionRow]) {
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for row in rows {
        *counts.entry(row.sigma_source.as_str()).or_default() += 1;
    }
    for (source, n) in counts {
        println!("  {} rows: {}", source, n);
    }
}

fn dedupe_trade_rows(rows: &[DecisionRow]) -> Vec<DecisionRow> {
    let mut first_by_key: BTreeMap<TradeOpportunityKey, DecisionRow> = BTreeMap::new();
    for row in rows {
        if let Some(key) = TradeOpportunityKey::from_row(row) {
            first_by_key.entry(key).or_insert_with(|| row.clone());
        }
    }
    first_by_key.into_values().collect()
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct TradeOpportunityKey {
    ticker: String,
    side: String,
    limit_price: String,
    contracts: u32,
}

impl TradeOpportunityKey {
    fn from_row(row: &DecisionRow) -> Option<Self> {
        if !is_trade_row(row) {
            return None;
        }
        Some(Self {
            ticker: row.ticker.clone(),
            side: row.side.as_deref()?.to_ascii_lowercase(),
            limit_price: row.limit_price?.normalize().to_string(),
            contracts: row.contracts?,
        })
    }
}

async fn fetch_and_join_outcomes(rows: &[DecisionRow]) -> Result<Vec<JoinedDecision>> {
    let mut station_ranges: BTreeMap<&str, (NaiveDate, NaiveDate)> = BTreeMap::new();
    for row in rows {
        station_ranges
            .entry(row.settlement_station.as_str())
            .and_modify(|(min, max)| {
                *min = (*min).min(row.resolution_date);
                *max = (*max).max(row.resolution_date);
            })
            .or_insert((row.resolution_date, row.resolution_date));
    }

    let client = IemCliClient::iem_default();
    let mut cli = Vec::new();
    for (station, (start, end)) in station_ranges {
        cli.extend(fetch_cli_for_range(&client, station, start, end).await?);
    }

    Ok(rows
        .iter()
        .filter_map(|row| join_outcome(row, &cli))
        .collect())
}

async fn fetch_cli_for_range(
    client: &IemCliClient,
    station_icao: &str,
    start: NaiveDate,
    end: NaiveDate,
) -> Result<Vec<weather_forecast::CliReport>> {
    let mut out = Vec::new();
    let (mut y, mut m) = (start.year(), start.month());
    let end_y = end.year();
    let end_m = end.month();
    loop {
        let chunk = client
            .fetch_month(station_icao, y, m)
            .await
            .with_context(|| format!("IEM CLI fetch failed for {station_icao} {y}-{m:02}"))?;
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

async fn fetch_and_estimate_pnl(
    kalshi_base_url: &str,
    trade_rows: &[DecisionRow],
    interval: PeriodInterval,
) -> Result<Vec<PnlEstimate>> {
    if trade_rows.is_empty() {
        return Ok(Vec::new());
    }

    let mut by_ticker: BTreeMap<&str, Vec<&DecisionRow>> = BTreeMap::new();
    for row in trade_rows {
        by_ticker.entry(row.ticker.as_str()).or_default().push(row);
    }

    let client = KalshiClient::new(kalshi_base_url.to_string());
    let mut out = Vec::new();
    for (ticker, rows) in by_ticker {
        let Some(series) = series_ticker_from_market_ticker(ticker) else {
            eprintln!("skipping {ticker}: cannot derive series ticker");
            continue;
        };
        let (start, end) = candle_fetch_window(&rows, interval);
        let candles = match client
            .fetch_candlesticks(series, ticker, start.timestamp(), end.timestamp(), interval)
            .await
        {
            Ok(c) => c,
            Err(e) => {
                eprintln!("candles fetch failed for {ticker}: {e}");
                continue;
            }
        };
        for row in rows {
            if let Some(candle) = candle_containing_ts(&candles, row.ts, interval) {
                if let Some(pnl) = estimate_trade_pnl(row, candle) {
                    out.push(pnl);
                }
            }
        }
    }
    Ok(out)
}

fn candle_fetch_window(
    rows: &[&DecisionRow],
    interval: PeriodInterval,
) -> (DateTime<Utc>, DateTime<Utc>) {
    let min_ts = rows.iter().map(|r| r.ts).min().expect("non-empty rows");
    let max_ts = rows.iter().map(|r| r.ts).max().expect("non-empty rows");
    let pad = Duration::seconds(i64::from(interval.as_minutes()) * 60);
    (min_ts - pad, max_ts + pad)
}

fn print_metrics_line(source: &str, m: &weather_backtest::Metrics) {
    println!(
        "  {source}: n={} hit={:.4} brier={:.4} log_loss={:.4} temp_bias={:+.3}F",
        m.n, m.hit_rate, m.mean_brier, m.mean_log_loss, m.mean_temp_bias_f
    );
    println!("    bucket   n      mean_pred   realised   diff");
    for (i, b) in m.calibration.iter().enumerate() {
        if b.n == 0 {
            continue;
        }
        let lo = i as f64 * 0.1;
        let hi = (i + 1) as f64 * 0.1;
        println!(
            "    [{lo:.1},{hi:.1}) {:>5}   {:.4}      {:.4}    {:+.4}",
            b.n,
            b.mean_predicted_p,
            b.realised_yes_rate,
            b.realised_yes_rate - b.mean_predicted_p
        );
    }
}

fn print_pnl_line(source: &str, s: &weather_backtest::PnlSummary) {
    println!(
        "  {source}: opportunities={} marked={} missing={} net_total=${} mean_net/contract={:+.4} positive_mark_rate={:.4}",
        s.n_trades,
        s.n_marked,
        s.n_missing_mark,
        s.net_total.round_dp(4),
        s.mean_net_per_contract,
        s.win_rate
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expands_jsonl_files_in_directory() {
        let dir = std::env::temp_dir().join(format!(
            "weather-backtest-replay-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join("a.jsonl"), "").unwrap();
        std::fs::write(dir.join("b.txt"), "").unwrap();

        let paths = expand_decision_paths(std::slice::from_ref(&dir)).unwrap();
        assert_eq!(paths, vec![dir.join("a.jsonl")]);

        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn period_maps_to_scanner_interval() {
        assert_eq!(PeriodInterval::from(ReplayPeriod::Minute).as_minutes(), 1);
        assert_eq!(PeriodInterval::from(ReplayPeriod::Hour).as_minutes(), 60);
        assert_eq!(PeriodInterval::from(ReplayPeriod::Day).as_minutes(), 1440);
    }

    #[test]
    fn dedupe_trade_rows_collapses_repeated_dry_run_emissions() {
        let row = decision_row("KXHIGHNY-26MAY04-T70", "yes", "0.45", 10);
        let later_same = DecisionRow {
            ts: row.ts + Duration::seconds(5),
            ..row.clone()
        };
        let different_price = decision_row("KXHIGHNY-26MAY04-T70", "yes", "0.46", 10);
        let deduped = dedupe_trade_rows(&[row, later_same, different_price]);
        assert_eq!(deduped.len(), 2);
    }

    #[test]
    fn describe_outcome_filters_lists_active_flags() {
        assert_eq!(describe_outcome_filters(false, false), None);
        assert_eq!(
            describe_outcome_filters(true, false).as_deref(),
            Some("--trades-only")
        );
        assert_eq!(
            describe_outcome_filters(false, true).as_deref(),
            Some("--by-opportunity")
        );
        assert_eq!(
            describe_outcome_filters(true, true).as_deref(),
            Some("--trades-only, --by-opportunity")
        );
    }

    #[test]
    fn apply_outcome_filters_composes_trades_only_and_dedup() {
        let date = NaiveDate::from_ymd_opt(2026, 5, 4).unwrap();
        let mk = |ticker: &str, decision: &str, ts_offset: i64| {
            let mut r = decision_row(ticker, "yes", "0.45", 10);
            r.decision = decision.to_string();
            r.resolution_date = date;
            r.ts += Duration::seconds(ts_offset);
            JoinedDecision {
                row: r,
                realised_temp_f: 80,
                realised_yes: true,
            }
        };
        let trade_early = mk("KXHIGHNY-26MAY04-T70", "trade", 0);
        let trade_late = mk("KXHIGHNY-26MAY04-T70", "trade", 60);
        let no_trade = mk("KXHIGHNY-26MAY04-T71", "no_trade", 0);

        // No flags: every row passes through.
        let none = apply_outcome_filters(
            &[trade_early.clone(), trade_late.clone(), no_trade.clone()],
            false,
            false,
        );
        assert_eq!(none.len(), 3);

        // trades_only: no_trade dropped.
        let trades = apply_outcome_filters(
            &[trade_early.clone(), trade_late.clone(), no_trade.clone()],
            true,
            false,
        );
        assert_eq!(trades.len(), 2);

        // by_opportunity: same (ticker, date) collapsed.
        let opps = apply_outcome_filters(
            &[trade_early.clone(), trade_late.clone(), no_trade.clone()],
            false,
            true,
        );
        assert_eq!(opps.len(), 2);

        // Combined: collapse to one trade opportunity.
        let combined = apply_outcome_filters(&[trade_early, trade_late, no_trade], true, true);
        assert_eq!(combined.len(), 1);
        assert_eq!(combined[0].row.decision, "trade");
    }

    #[test]
    fn candle_window_pads_on_both_sides() {
        let ts = DateTime::parse_from_rfc3339("2026-05-04T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let row = decision_row("KXHIGHNY-26MAY04-T70", "yes", "0.45", 10);
        let rows = vec![&row];
        let (start, end) = candle_fetch_window(&rows, PeriodInterval::OneHour);
        assert_eq!(start, ts - Duration::hours(1));
        assert_eq!(end, ts + Duration::hours(1));
    }

    fn decision_row(ticker: &str, side: &str, limit_price: &str, contracts: u32) -> DecisionRow {
        DecisionRow {
            ts: DateTime::parse_from_rfc3339("2026-05-04T12:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
            ticker: ticker.to_string(),
            city: "NY".to_string(),
            settlement_station: "KNYC".to_string(),
            stat: weather_types::TempStat::DailyHigh,
            direction: weather_types::ThresholdDirection::AtOrAbove,
            strike_temperature_f: 70,
            resolution_date: NaiveDate::from_ymd_opt(2026, 5, 4).unwrap(),
            horizon_days: 0,
            forecast_temp_f: 72,
            sigma_f: 2.0,
            sigma_source: "static".to_string(),
            model_p_yes: rust_decimal_macros::dec!(0.60),
            yes_bid: Some(rust_decimal_macros::dec!(0.40)),
            yes_ask: Some(rust_decimal_macros::dec!(0.45)),
            spread: Some(rust_decimal_macros::dec!(0.05)),
            decision: "trade".to_string(),
            reason: None,
            side: Some(side.to_string()),
            limit_price: Some(limit_price.parse().unwrap()),
            contracts: Some(contracts),
            fee_estimate: Some(rust_decimal_macros::dec!(0.02)),
            raw_edge: Some(rust_decimal_macros::dec!(0.15)),
            net_ev_per_contract: Some(rust_decimal_macros::dec!(0.13)),
            market_p_implied: Some(rust_decimal_macros::dec!(0.45)),
        }
    }
}
