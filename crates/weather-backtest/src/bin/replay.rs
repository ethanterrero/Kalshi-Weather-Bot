//! `replay` — JSONL decision-log replay with realised outcomes + P&L.
//!
//! Reads `logs/decisions/*.jsonl` (the bot's per-pass decision log) and
//! joins each row to IEM CLI for realised daily high/low (settlement
//! ground truth) and to Kalshi `/candles` for the close price at each
//! Trade decision's timestamp (estimated fill). Then reports calibration
//! metrics split by `sigma_source` plus an aggregate P&L estimate over
//! the Trade rows.
//!
//! Why two metrics tracks (calibration + P&L): a model can be calibrated
//! and still lose to fees if its edges are too small. Per-Perplexity:
//! Phase A criterion (a) "net-of-fee expectancy >0 per trade" is the
//! gating metric, and you can't compute it from JSONL alone — needed
//! candle close prices, which is what PR #19 added.
//!
//! Example:
//!
//!   cargo run -p weather-backtest --bin replay -- \
//!       --jsonl-dir logs/decisions/
//!
//! Optional `--no-network` skips the CLI + candles fetches and prints
//! only the calibration metrics that can be computed from the JSONL on
//! its own (no realised outcomes, no P&L).

use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, Datelike, Utc};
use clap::Parser;
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use std::collections::HashMap;
use std::path::PathBuf;

use weather_backtest::{join_outcome, metrics, parse_decisions_text, DecisionRow, JoinedDecision};
use weather_forecast::{CliReport, IemCliClient};
use weather_scanner::{Candlestick, KalshiClient, PeriodInterval};

#[derive(Debug, Parser)]
#[command(
    name = "replay",
    about = "Replay JSONL decisions against realised outcomes + Kalshi close prices"
)]
struct Args {
    /// Directory of `YYYY-MM-DD.jsonl` files written by the bot. The
    /// directory is walked non-recursively.
    #[arg(long)]
    jsonl_dir: PathBuf,
    /// Kalshi REST base URL. Defaults to demo (paper-trading); pass
    /// `https://trading-api.kalshi.com/trade-api/v2` to read prod
    /// candles.
    #[arg(long, default_value = "https://demo-api.kalshi.co/trade-api/v2")]
    kalshi_base: String,
    /// Skip CLI + candles fetches (offline mode). Useful when iterating
    /// on the metrics output without burning rate limits.
    #[arg(long, default_value_t = false)]
    no_network: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    println!("loading JSONL from {}...", args.jsonl_dir.display());
    let decisions = read_all_jsonl(&args.jsonl_dir)?;
    if decisions.is_empty() {
        return Err(anyhow!(
            "no decision rows found in {} — is this the right directory?",
            args.jsonl_dir.display()
        ));
    }
    println!("  → {} total rows", decisions.len());

    // Always-on summary regardless of network access.
    let n_trade = decisions.iter().filter(|r| r.decision == "trade").count();
    let n_no_trade = decisions.len() - n_trade;
    let n_static = decisions
        .iter()
        .filter(|r| r.sigma_source == "static")
        .count();
    let n_gefs = decisions
        .iter()
        .filter(|r| r.sigma_source == "gefs_ensemble")
        .count();
    println!("  → trade={}, no_trade={}", n_trade, n_no_trade);
    println!(
        "  → sigma_source: static={}, gefs_ensemble={}",
        n_static, n_gefs
    );

    if args.no_network {
        println!();
        println!("--- offline mode: only printing per-source row counts ---");
        return Ok(());
    }

    // ── Realised outcomes from IEM CLI ────────────────────────────────────
    println!();
    println!("fetching IEM CLI for realised outcomes...");
    let cli_reports = fetch_cli_for_decisions(&decisions)
        .await
        .context("CLI fetch failed")?;
    println!("  → {} CLI rows", cli_reports.len());

    // Join every decision to its CLI outcome.
    let joined: Vec<JoinedDecision> = decisions
        .iter()
        .filter_map(|d| join_outcome(d, &cli_reports))
        .collect();
    println!(
        "  → {} decisions joined to CLI ({} unmatched)",
        joined.len(),
        decisions.len() - joined.len()
    );
    if joined.is_empty() {
        return Err(anyhow!(
            "no decision rows had a matching CLI report — check that resolution_date and \
             settlement_station match what IEM serves for these stations"
        ));
    }

    // ── Calibration metrics, split by sigma_source ────────────────────────
    println!();
    print_calibration_summary("ALL ROWS", &joined);
    let gefs: Vec<JoinedDecision> = joined
        .iter()
        .filter(|j| j.row.sigma_source == "gefs_ensemble")
        .cloned()
        .collect();
    let stat: Vec<JoinedDecision> = joined
        .iter()
        .filter(|j| j.row.sigma_source == "static")
        .cloned()
        .collect();
    if !gefs.is_empty() {
        println!();
        print_calibration_summary("sigma_source = gefs_ensemble", &gefs);
    }
    if !stat.is_empty() {
        println!();
        print_calibration_summary("sigma_source = static", &stat);
    }

    // ── P&L on Trade rows, using Kalshi candles for close prices ──────────
    let trade_rows: Vec<&JoinedDecision> = joined
        .iter()
        .filter(|j| j.row.decision == "trade")
        .collect();
    if trade_rows.is_empty() {
        println!();
        println!("--- no Trade rows; skipping P&L ---");
        return Ok(());
    }
    println!();
    println!(
        "fetching Kalshi candles for {} Trade rows from {}...",
        trade_rows.len(),
        args.kalshi_base
    );
    let kalshi = KalshiClient::new(args.kalshi_base.clone());
    let candles_by_ticker = fetch_candles_for_trades(&kalshi, &trade_rows)
        .await
        .context("Kalshi candles fetch failed")?;
    let pnl_rows: Vec<TradePnl> = trade_rows
        .iter()
        .filter_map(|j| compute_trade_pnl(j, &candles_by_ticker))
        .collect();
    println!(
        "  → {} Trade rows with usable fill estimates ({} skipped)",
        pnl_rows.len(),
        trade_rows.len() - pnl_rows.len()
    );

    print_pnl_summary("ALL TRADES", &pnl_rows);
    let gefs_pnl: Vec<TradePnl> = pnl_rows
        .iter()
        .filter(|p| p.sigma_source == "gefs_ensemble")
        .cloned()
        .collect();
    let stat_pnl: Vec<TradePnl> = pnl_rows
        .iter()
        .filter(|p| p.sigma_source == "static")
        .cloned()
        .collect();
    if !gefs_pnl.is_empty() {
        print_pnl_summary("sigma_source = gefs_ensemble", &gefs_pnl);
    }
    if !stat_pnl.is_empty() {
        print_pnl_summary("sigma_source = static", &stat_pnl);
    }

    Ok(())
}

// ── JSONL ingestion ──────────────────────────────────────────────────────

fn read_all_jsonl(dir: &PathBuf) -> Result<Vec<DecisionRow>> {
    let mut rows = Vec::new();
    let entries = std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))?;
    let mut paths: Vec<_> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("jsonl"))
        .collect();
    paths.sort();
    for path in paths {
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        let chunk =
            parse_decisions_text(&text).with_context(|| format!("parsing {}", path.display()))?;
        rows.extend(chunk);
    }
    Ok(rows)
}

// ── IEM CLI fetch ────────────────────────────────────────────────────────

async fn fetch_cli_for_decisions(decisions: &[DecisionRow]) -> Result<Vec<CliReport>> {
    let client = IemCliClient::iem_default();
    // Build the set of (station, year, month) tuples we need so we hit
    // each month exactly once.
    let mut wanted: std::collections::BTreeSet<(String, i32, u32)> = Default::default();
    for d in decisions {
        wanted.insert((
            d.settlement_station.clone(),
            d.resolution_date.year(),
            d.resolution_date.month(),
        ));
    }
    let mut out = Vec::new();
    for (station, year, month) in wanted {
        let chunk = client
            .fetch_month(&station, year, month)
            .await
            .with_context(|| format!("CLI fetch for {} {}-{}", station, year, month))?;
        out.extend(chunk);
    }
    Ok(out)
}

// ── Kalshi candles fetch ─────────────────────────────────────────────────

async fn fetch_candles_for_trades(
    kalshi: &KalshiClient,
    trade_rows: &[&JoinedDecision],
) -> Result<HashMap<String, Vec<Candlestick>>> {
    // For each ticker, find the [min_ts, max_ts] window the trade rows
    // span, fetch hourly candles over that window. One fetch per ticker.
    let mut by_ticker: HashMap<String, Vec<&JoinedDecision>> = HashMap::new();
    for j in trade_rows {
        by_ticker.entry(j.row.ticker.clone()).or_default().push(j);
    }

    let mut out = HashMap::new();
    for (ticker, rows) in by_ticker {
        let series = series_ticker_from(&ticker)
            .ok_or_else(|| anyhow!("unparseable ticker '{}'", ticker))?;
        let min_ts = rows.iter().map(|j| j.row.ts.timestamp()).min().unwrap();
        let max_ts = rows.iter().map(|j| j.row.ts.timestamp()).max().unwrap();
        // Pad by one hour each side so the candle-containing-ts lookup
        // always finds something.
        let candles = kalshi
            .fetch_candlesticks(
                &series,
                &ticker,
                min_ts - 3600,
                max_ts + 3600,
                PeriodInterval::OneHour,
            )
            .await
            .with_context(|| format!("candles fetch for {}", ticker))?;
        out.insert(ticker, candles);
    }
    Ok(out)
}

/// Strip the date + strike suffix off a Kalshi market ticker to get the
/// series ticker. `KXHIGHNY-26JUL04-T75` → `KXHIGHNY`.
fn series_ticker_from(ticker: &str) -> Option<String> {
    ticker.split('-').next().map(|s| s.to_string())
}

// ── P&L computation ──────────────────────────────────────────────────────

/// Aggregate-ready record per Trade row. Several fields are kept for
/// hypothetical per-trade detail output and future filtering even though
/// the v1 summary only reads a subset; mark `dead_code` rather than
/// dropping them so a follow-up `--per-trade` flag can populate.
#[derive(Debug, Clone)]
#[allow(dead_code)]
struct TradePnl {
    ticker: String,
    sigma_source: String,
    side: String, // "yes" | "no"
    contracts: u32,
    /// What the bot's strategy proposed at decision time.
    intended_limit_price: Decimal,
    /// What the candle says the resting opposite-side ask was at decision
    /// time. This is the price we'd actually pay.
    estimated_fill_price: Decimal,
    fee_per_contract: Decimal,
    realised_yes: bool,
    /// Net P&L per contract: payoff − fill − fee. Positive = profitable.
    pnl_per_contract: Decimal,
    /// pnl_per_contract × contracts.
    total_pnl: Decimal,
}

fn compute_trade_pnl(
    j: &JoinedDecision,
    candles_by_ticker: &HashMap<String, Vec<Candlestick>>,
) -> Option<TradePnl> {
    let row = &j.row;
    let candles = candles_by_ticker.get(&row.ticker)?;
    let candle = find_candle_for_ts(candles, row.ts)?;

    let side = row.side.as_deref()?;
    let contracts = row.contracts?;
    let intended = row.limit_price?;
    let fee = row.fee_estimate?;

    // For a YES trade we pay the yes_ask close; for a NO trade the
    // implied buy is at (1 - yes_bid_close). This matches the live bot's
    // post-only-at-the-resting-opposite-side convention.
    let estimated_fill = match side {
        "yes" => candle.yes_ask.close,
        "no" => Decimal::ONE - candle.yes_bid.close,
        _ => return None,
    };

    // Payoff: $1 if our side wins, $0 if it doesn't.
    let won = match side {
        "yes" => j.realised_yes,
        "no" => !j.realised_yes,
        _ => return None,
    };
    let payoff = if won { Decimal::ONE } else { Decimal::ZERO };
    let pnl_per_contract = payoff - estimated_fill - fee;
    let total_pnl = pnl_per_contract * Decimal::from(contracts);

    Some(TradePnl {
        ticker: row.ticker.clone(),
        sigma_source: row.sigma_source.clone(),
        side: side.to_string(),
        contracts,
        intended_limit_price: intended,
        estimated_fill_price: estimated_fill,
        fee_per_contract: fee,
        realised_yes: j.realised_yes,
        pnl_per_contract,
        total_pnl,
    })
}

/// Find the candle whose period contains `ts` (i.e., the candle whose
/// `end_period_ts` is the smallest value ≥ `ts`). Falls back to the
/// nearest candle if none are after `ts` (rare; happens when trade fired
/// in the final period and `end_period_ts` rounds backwards).
fn find_candle_for_ts(candles: &[Candlestick], ts: DateTime<Utc>) -> Option<&Candlestick> {
    let target = ts.timestamp();
    candles
        .iter()
        .filter(|c| c.end_period_ts >= target)
        .min_by_key(|c| c.end_period_ts)
        .or_else(|| candles.iter().max_by_key(|c| c.end_period_ts))
}

// ── Output ───────────────────────────────────────────────────────────────

fn print_calibration_summary(label: &str, joined: &[JoinedDecision]) {
    let m = metrics(joined);
    println!("=== Calibration: {} (n={}) ===", label, m.n);
    if m.n == 0 {
        return;
    }
    println!("  hit rate:           {:.4}", m.hit_rate);
    println!(
        "  mean Brier:         {:.4}  (lower is better; 0.25 = chance)",
        m.mean_brier
    );
    println!("  mean log loss:      {:.4}", m.mean_log_loss);
    println!(
        "  mean temp bias °F:  {:+.3}  (positive = forecast > realised)",
        m.mean_temp_bias_f
    );
    println!("  calibration buckets:");
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
}

fn print_pnl_summary(label: &str, rows: &[TradePnl]) {
    println!();
    println!("=== P&L: {} (n={}) ===", label, rows.len());
    if rows.is_empty() {
        return;
    }
    let n = rows.len();
    let total: Decimal = rows.iter().map(|r| r.total_pnl).sum();
    let mean_per_trade: Decimal =
        rows.iter().map(|r| r.pnl_per_contract).sum::<Decimal>() / Decimal::from(n);
    let wins = rows
        .iter()
        .filter(|r| r.pnl_per_contract > Decimal::ZERO)
        .count();
    let win_rate = wins as f64 / n as f64;
    let mean_slippage: f64 = rows
        .iter()
        .map(|r| {
            (r.estimated_fill_price - r.intended_limit_price)
                .to_f64()
                .unwrap_or(0.0)
        })
        .sum::<f64>()
        / n as f64;

    println!("  total trades:              {}", n);
    println!(
        "  wins:                      {}  ({:.2}%)",
        wins,
        win_rate * 100.0
    );
    println!("  total P&L (estimated):     ${}", total);
    println!("  mean P&L per contract:     ${}", mean_per_trade);
    println!(
        "  mean slippage (fill−lim):  {:+.4}  (positive = paid more than the bot intended)",
        mean_slippage
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use weather_scanner::{BookOhlc, TradeOhlc};

    fn book(close: &str) -> BookOhlc {
        let v = close.parse().unwrap();
        BookOhlc {
            open: v,
            high: v,
            low: v,
            close: v,
        }
    }

    fn candle(end_ts: i64, ask_close: &str, bid_close: &str) -> Candlestick {
        Candlestick {
            end_period_ts: end_ts,
            price: None::<TradeOhlc>,
            yes_ask: book(ask_close),
            yes_bid: book(bid_close),
            volume_dollars: Decimal::ZERO,
            open_interest_dollars: Decimal::ZERO,
        }
    }

    #[test]
    fn series_ticker_strip_handles_typical_kalshi_format() {
        assert_eq!(
            series_ticker_from("KXHIGHNY-26JUL04-T75"),
            Some("KXHIGHNY".to_string())
        );
        assert_eq!(
            series_ticker_from("KXLOWCHI-26DEC15-T20"),
            Some("KXLOWCHI".to_string())
        );
    }

    #[test]
    fn find_candle_for_ts_picks_smallest_end_geq_ts() {
        let cs = vec![
            candle(1700000000, "0.50", "0.40"),
            candle(1700003600, "0.55", "0.45"),
            candle(1700007200, "0.60", "0.50"),
        ];
        // ts is between candle[0] and candle[1]'s end → should pick [1].
        let target = Utc.timestamp_opt(1700001800, 0).unwrap();
        let c = find_candle_for_ts(&cs, target).unwrap();
        assert_eq!(c.end_period_ts, 1700003600);
    }

    #[test]
    fn find_candle_falls_back_to_last_when_ts_is_after_all_candles() {
        let cs = vec![
            candle(1700000000, "0.50", "0.40"),
            candle(1700003600, "0.55", "0.45"),
        ];
        let target = Utc.timestamp_opt(1700100000, 0).unwrap();
        let c = find_candle_for_ts(&cs, target).unwrap();
        assert_eq!(c.end_period_ts, 1700003600);
    }
}
