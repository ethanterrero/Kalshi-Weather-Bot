//! Replay decisions against realised CLI outcomes.
//!
//! Inputs:
//!   - JSONL decision log emitted by `weather-bot` (one row per market per pass).
//!   - CLI reports for the same (station, date) pulled via
//!     `weather-forecast::cli::IemCliClient`.
//!
//! Outputs:
//!   - `JoinedDecision` per row — the original decision plus realised
//!     YES/NO and the corresponding CLI extreme.
//!   - `Metrics` aggregated across joined rows: hit rate, mean Brier
//!     score, mean log loss, μ-error, plus calibration buckets.
//!
//! The `DecisionRow` here mirrors the JSONL schema but only the
//! deserialise side — we never write JSONL from this crate. Keeping it
//! decoupled means the bot can ship schema changes without retiring
//! historical files we still want to read (we just `#[serde(default)]`
//! the new fields when that day comes; today the schema is fresh enough
//! we don't need it yet).

pub mod historical;
pub mod reconcile;

pub use historical::{historical_calibration, model_p_yes, CalibrationPlan};
pub use reconcile::{
    reconcile_settled_market, KalshiOutcome, ReconcileOutcome, ReconcileResult, SkipReason,
};

use chrono::{DateTime, NaiveDate, Utc};
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use serde::Deserialize;
use std::{collections::BTreeMap, path::Path};
use thiserror::Error;

use weather_forecast::CliReport;
use weather_scanner::{Candlestick, PeriodInterval};
use weather_types::{TempStat, ThresholdDirection};

#[derive(Debug, Error)]
pub enum BacktestError {
    #[error("I/O reading JSONL: {0}")]
    Io(#[from] std::io::Error),
    #[error("malformed JSONL line {line_no}: {source}")]
    Json {
        line_no: usize,
        #[source]
        source: serde_json::Error,
    },
}

/// Mirror of `weather_bot::decision_log::DecisionRecord`. Deserialise-only;
/// we never write these. Field names must match the JSONL schema.
#[derive(Debug, Clone, Deserialize)]
pub struct DecisionRow {
    pub ts: DateTime<Utc>,
    pub ticker: String,
    pub city: String,
    pub settlement_station: String,
    pub stat: TempStat,
    pub direction: ThresholdDirection,
    pub strike_temperature_f: i32,
    pub resolution_date: NaiveDate,
    pub horizon_days: i64,
    pub forecast_temp_f: i32,
    pub sigma_f: f64,
    /// `"gefs_ensemble"` or `"static"`. See `weather_pricing::ModelPricing::sigma_source`.
    pub sigma_source: String,
    pub model_p_yes: Decimal,
    pub yes_bid: Option<Decimal>,
    pub yes_ask: Option<Decimal>,
    pub spread: Option<Decimal>,
    pub decision: String,
    pub reason: Option<String>,
    pub side: Option<String>,
    pub limit_price: Option<Decimal>,
    pub contracts: Option<u32>,
    pub fee_estimate: Option<Decimal>,
    pub raw_edge: Option<Decimal>,
    pub net_ev_per_contract: Option<Decimal>,
    pub market_p_implied: Option<Decimal>,
    /// Optional risk disposition tag from the strategy loop (`approved`,
    /// `adjusted_*`, `rejected_*`). Newer JSONL rows include this; older
    /// logs omit it.
    #[serde(default)]
    pub risk_outcome: Option<String>,
    /// Optional execution disposition tag from the strategy loop
    /// (`dry_run_suppressed`, `paper_submitted`, etc.). Used to separate
    /// strategy intent from actual execution attempts in replay diagnostics.
    #[serde(default)]
    pub execution_outcome: Option<String>,
}

/// Read a single JSONL file into a `Vec<DecisionRow>`. Each line is a
/// complete record; blank lines and trailing newlines are tolerated.
pub fn read_decisions(path: impl AsRef<Path>) -> Result<Vec<DecisionRow>, BacktestError> {
    let text = std::fs::read_to_string(path)?;
    parse_decisions_text(&text)
}

/// Same as `read_decisions` but takes the JSONL text directly. Easier to
/// fixture in tests.
pub fn parse_decisions_text(text: &str) -> Result<Vec<DecisionRow>, BacktestError> {
    let mut out = Vec::new();
    for (idx, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let row: DecisionRow = serde_json::from_str(line).map_err(|e| BacktestError::Json {
            line_no: idx + 1,
            source: e,
        })?;
        out.push(row);
    }
    Ok(out)
}

/// A `DecisionRow` paired with the realised CLI outcome and the YES-side
/// resolution for the row's threshold.
#[derive(Debug, Clone)]
pub struct JoinedDecision {
    pub row: DecisionRow,
    /// CLI extreme (high or low, matching `row.stat`) on the resolution day.
    pub realised_temp_f: i32,
    /// Whether YES resolved true given the threshold direction and the
    /// realised CLI extreme.
    pub realised_yes: bool,
}

/// Look up the CLI extreme on the row's resolution date and compute
/// realised YES. Returns `None` when no matching CLI is found (skip rows
/// for which we don't have ground truth).
pub fn join_outcome(row: &DecisionRow, cli: &[CliReport]) -> Option<JoinedDecision> {
    let report = cli
        .iter()
        .find(|r| r.station == row.settlement_station && r.date == row.resolution_date)?;
    let temp_f = match row.stat {
        TempStat::DailyHigh => report.high_f?,
        TempStat::DailyLow => report.low_f?,
    };
    let realised_yes = realised_yes_for(row.direction, row.strike_temperature_f, temp_f);
    Some(JoinedDecision {
        row: row.clone(),
        realised_temp_f: temp_f,
        realised_yes,
    })
}

/// Whether YES resolves true: `direction(strike, realised)`.
pub fn realised_yes_for(direction: ThresholdDirection, strike_f: i32, realised_f: i32) -> bool {
    match direction {
        ThresholdDirection::AtOrAbove => realised_f >= strike_f,
        ThresholdDirection::AtOrBelow => realised_f <= strike_f,
    }
}

/// Headline metrics across a set of joined decisions. Computed only over
/// rows where the model produced a probability (every row does today, but
/// the future schema may include rows where it didn't).
#[derive(Debug, Clone, PartialEq)]
pub struct Metrics {
    pub n: usize,
    /// Fraction of rows where the model's "leaning" matched the outcome.
    /// Leaning = `model_p_yes >= 0.5` → YES-leaning.
    pub hit_rate: f64,
    /// Mean Brier score: `(p - y)^2`, lower is better. 0.0 = perfect,
    /// 0.25 = chance for binary outcomes.
    pub mean_brier: f64,
    /// Mean log loss: `-y*ln(p) - (1-y)*ln(1-p)`. Lower is better. We
    /// clamp `p` away from 0/1 by `epsilon` to avoid -inf.
    pub mean_log_loss: f64,
    /// Mean signed forecast-temp error: `forecast_temp_f - realised_temp_f`.
    /// Positive = we predicted hotter than reality.
    pub mean_temp_bias_f: f64,
    /// 10-bucket calibration: index `i` covers `model_p_yes ∈ [i*0.1, (i+1)*0.1)`,
    /// last bucket includes `1.0`. Each bucket carries (n_in_bucket,
    /// realised_yes_rate, mean_predicted_p). Buckets with `n=0` show
    /// `realised_yes_rate=0.0` and `mean_predicted_p=0.0`.
    pub calibration: [CalibrationBucket; 10],
}

#[derive(Debug, Clone, PartialEq)]
pub struct CalibrationBucket {
    pub n: usize,
    pub realised_yes_rate: f64,
    pub mean_predicted_p: f64,
}

const LOG_LOSS_EPSILON: f64 = 1e-6;

/// Aggregate metrics across joined decisions. An empty input yields
/// zeroed metrics with `n=0`.
pub fn metrics(joined: &[JoinedDecision]) -> Metrics {
    let n = joined.len();
    if n == 0 {
        return Metrics {
            n: 0,
            hit_rate: 0.0,
            mean_brier: 0.0,
            mean_log_loss: 0.0,
            mean_temp_bias_f: 0.0,
            calibration: std::array::from_fn(|_| CalibrationBucket {
                n: 0,
                realised_yes_rate: 0.0,
                mean_predicted_p: 0.0,
            }),
        };
    }

    let mut hits = 0usize;
    let mut brier_sum = 0.0;
    let mut log_loss_sum = 0.0;
    let mut bias_sum = 0i64;
    // (n, sum_realised_yes, sum_predicted_p) per bucket.
    let mut bins: [(usize, f64, f64); 10] = [(0, 0.0, 0.0); 10];

    for j in joined {
        let p = j.row.model_p_yes.to_f64().unwrap_or(0.0).clamp(0.0, 1.0);
        let y = if j.realised_yes { 1.0 } else { 0.0 };

        // Hit rate uses the model's leaning at 0.5.
        let model_says_yes = p >= 0.5;
        if model_says_yes == j.realised_yes {
            hits += 1;
        }

        brier_sum += (p - y).powi(2);
        let p_clamped = p.clamp(LOG_LOSS_EPSILON, 1.0 - LOG_LOSS_EPSILON);
        log_loss_sum += -(y * p_clamped.ln() + (1.0 - y) * (1.0 - p_clamped).ln());

        bias_sum += (j.row.forecast_temp_f - j.realised_temp_f) as i64;

        let bin = ((p * 10.0).floor() as usize).min(9);
        bins[bin].0 += 1;
        bins[bin].1 += y;
        bins[bin].2 += p;
    }

    let calibration: [CalibrationBucket; 10] = std::array::from_fn(|i| {
        let (count, sum_y, sum_p) = bins[i];
        if count == 0 {
            CalibrationBucket {
                n: 0,
                realised_yes_rate: 0.0,
                mean_predicted_p: 0.0,
            }
        } else {
            CalibrationBucket {
                n: count,
                realised_yes_rate: sum_y / count as f64,
                mean_predicted_p: sum_p / count as f64,
            }
        }
    });

    Metrics {
        n,
        hit_rate: hits as f64 / n as f64,
        mean_brier: brier_sum / n as f64,
        mean_log_loss: log_loss_sum / n as f64,
        mean_temp_bias_f: bias_sum as f64 / n as f64,
        calibration,
    }
}

/// Reduce joined decisions to one per `(ticker, resolution_date)`, keeping
/// the earliest emission. Calibration aggregated over raw JSONL rows is
/// dominated by repeated dry-run emissions of the same market (one
/// opportunity emitted every pass for hours = thousands of rows that all
/// resolve identically). Dedupe before bucketing to make the histogram
/// meaningful as model evidence.
///
/// "Earliest emission" depends on input order. `read_decisions` returns
/// rows in file order; callers that want strict temporal order should
/// sort by `row.ts` before calling.
pub fn dedupe_joined_by_opportunity(joined: &[JoinedDecision]) -> Vec<JoinedDecision> {
    let mut first_by_key: BTreeMap<(String, NaiveDate), JoinedDecision> = BTreeMap::new();
    for j in joined {
        let key = (j.row.ticker.clone(), j.row.resolution_date);
        first_by_key.entry(key).or_insert_with(|| j.clone());
    }
    let mut out: Vec<JoinedDecision> = first_by_key.into_values().collect();
    out.sort_by_key(|j| j.row.ts);
    out
}

/// Filter joined decisions to only those the bot would actually act on
/// (`decision == "trade"`). Headline metrics over the whole priced
/// universe are dominated by far-tail no-trade rows where the model
/// correctly prices near 0 or 1; this restricts evaluation to the subset
/// where edge actually matters.
pub fn filter_joined_to_trades(joined: &[JoinedDecision]) -> Vec<JoinedDecision> {
    joined
        .iter()
        .filter(|j| is_trade_row(&j.row))
        .cloned()
        .collect()
}

/// Metrics grouped by the JSONL `sigma_source` tag. This is the cheap,
/// direct answer to "did GEFS σ help?" once the dry-run has accumulated
/// enough rows.
pub fn metrics_by_sigma_source(joined: &[JoinedDecision]) -> BTreeMap<String, Metrics> {
    let mut grouped: BTreeMap<String, Vec<JoinedDecision>> = BTreeMap::new();
    for j in joined {
        grouped
            .entry(j.row.sigma_source.clone())
            .or_default()
            .push(j.clone());
    }
    grouped
        .into_iter()
        .map(|(source, rows)| (source, metrics(&rows)))
        .collect()
}

/// P&L estimate for a single Trade row joined to a Kalshi candle.
///
/// This is mark-to-market, not realised fill accounting. Entry is the
/// JSONL limit price from the signal. Exit is conservative: long YES marks
/// to the candle's YES bid close; long NO marks to implied NO bid
/// (`1 - YES ask close`). Fees are subtracted once on entry because the
/// strategy's gate estimates entry fees only today.
#[derive(Debug, Clone, PartialEq)]
pub struct PnlEstimate {
    pub ticker: String,
    pub sigma_source: String,
    pub side: String,
    pub contracts: u32,
    pub entry_price: Decimal,
    pub mark_price: Decimal,
    pub fee_per_contract: Decimal,
    pub gross_per_contract: Decimal,
    pub net_per_contract: Decimal,
    pub net_total: Decimal,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PnlSummary {
    pub n_trades: usize,
    pub n_marked: usize,
    pub n_missing_mark: usize,
    pub net_total: Decimal,
    pub mean_net_per_contract: f64,
    pub win_rate: f64,
}

impl PnlSummary {
    pub fn empty(n_trades: usize, n_marked: usize) -> Self {
        Self {
            n_trades,
            n_marked,
            n_missing_mark: n_trades.saturating_sub(n_marked),
            net_total: Decimal::ZERO,
            mean_net_per_contract: 0.0,
            win_rate: 0.0,
        }
    }
}

pub fn summarize_pnl(estimates: &[PnlEstimate], n_trade_rows: usize) -> PnlSummary {
    if estimates.is_empty() {
        return PnlSummary::empty(n_trade_rows, 0);
    }
    let net_total = estimates.iter().map(|p| p.net_total).sum::<Decimal>();
    let total_contracts: u32 = estimates.iter().map(|p| p.contracts).sum();
    let net_per_contract_sum: f64 = estimates
        .iter()
        .map(|p| p.net_per_contract.to_f64().unwrap_or(0.0) * p.contracts as f64)
        .sum();
    let winners = estimates
        .iter()
        .filter(|p| p.net_per_contract > Decimal::ZERO)
        .count();
    PnlSummary {
        n_trades: n_trade_rows,
        n_marked: estimates.len(),
        n_missing_mark: n_trade_rows.saturating_sub(estimates.len()),
        net_total,
        mean_net_per_contract: if total_contracts == 0 {
            0.0
        } else {
            net_per_contract_sum / total_contracts as f64
        },
        win_rate: winners as f64 / estimates.len() as f64,
    }
}

pub fn summarize_pnl_by_sigma_source(
    estimates: &[PnlEstimate],
    trade_rows: &[DecisionRow],
) -> BTreeMap<String, PnlSummary> {
    let mut trade_counts: BTreeMap<String, usize> = BTreeMap::new();
    for row in trade_rows {
        if is_trade_row(row) {
            *trade_counts.entry(row.sigma_source.clone()).or_default() += 1;
        }
    }

    let mut grouped: BTreeMap<String, Vec<PnlEstimate>> = BTreeMap::new();
    for p in estimates {
        grouped
            .entry(p.sigma_source.clone())
            .or_default()
            .push(p.clone());
    }

    let mut out = BTreeMap::new();
    for (source, n_trades) in trade_counts {
        let rows = grouped.remove(&source).unwrap_or_default();
        out.insert(source, summarize_pnl(&rows, n_trades));
    }
    out
}

pub fn is_trade_row(row: &DecisionRow) -> bool {
    row.decision.eq_ignore_ascii_case("trade")
}

pub fn estimate_trade_pnl(row: &DecisionRow, candle: &Candlestick) -> Option<PnlEstimate> {
    if !is_trade_row(row) {
        return None;
    }
    let side = row.side.as_deref()?;
    let contracts = row.contracts?;
    let entry_price = row.limit_price?;
    let fee_per_contract = row.fee_estimate.unwrap_or(Decimal::ZERO);
    let mark_price = match side {
        "yes" | "Yes" | "YES" => candle.yes_bid.close,
        "no" | "No" | "NO" => Decimal::ONE - candle.yes_ask.close,
        _ => return None,
    };
    let gross_per_contract = mark_price - entry_price;
    let net_per_contract = gross_per_contract - fee_per_contract;
    Some(PnlEstimate {
        ticker: row.ticker.clone(),
        sigma_source: row.sigma_source.clone(),
        side: side.to_ascii_lowercase(),
        contracts,
        entry_price,
        mark_price,
        fee_per_contract,
        gross_per_contract,
        net_per_contract,
        net_total: net_per_contract * Decimal::from(contracts),
    })
}

/// Find the candle containing `ts`. Kalshi represents each candle by its
/// `end_period_ts`; the covered interval is `[end - interval, end)`.
pub fn candle_containing_ts(
    candles: &[Candlestick],
    ts: DateTime<Utc>,
    interval: PeriodInterval,
) -> Option<&Candlestick> {
    let ts = ts.timestamp();
    let width_secs = i64::from(interval.as_minutes()) * 60;
    candles
        .iter()
        .find(|c| ts >= c.end_period_ts - width_secs && ts < c.end_period_ts)
}

/// Kalshi weather market tickers start with the series ticker before the
/// first dash, e.g. `KXHIGHNY-26MAY02-T65` -> `KXHIGHNY`.
pub fn series_ticker_from_market_ticker(ticker: &str) -> Option<&str> {
    ticker.split_once('-').map(|(series, _)| series)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn row(
        stat: TempStat,
        dir: ThresholdDirection,
        strike: i32,
        date: NaiveDate,
        model_p: Decimal,
        forecast_temp: i32,
    ) -> DecisionRow {
        DecisionRow {
            ts: Utc::now(),
            ticker: "X".into(),
            city: "NY".into(),
            settlement_station: "KNYC".into(),
            stat,
            direction: dir,
            strike_temperature_f: strike,
            resolution_date: date,
            horizon_days: 1,
            forecast_temp_f: forecast_temp,
            sigma_f: 2.0,
            sigma_source: "static".to_string(),
            model_p_yes: model_p,
            yes_bid: None,
            yes_ask: None,
            spread: None,
            decision: "trade".into(),
            reason: None,
            side: Some("yes".into()),
            limit_price: None,
            contracts: None,
            fee_estimate: None,
            raw_edge: None,
            net_ev_per_contract: None,
            market_p_implied: None,
            risk_outcome: None,
            execution_outcome: None,
        }
    }

    fn cli(date: NaiveDate, high: Option<i32>, low: Option<i32>) -> CliReport {
        CliReport {
            station: "KNYC".into(),
            date,
            high_f: high,
            low_f: low,
        }
    }

    #[test]
    fn realised_yes_at_or_above_uses_inclusive_bound() {
        assert!(realised_yes_for(ThresholdDirection::AtOrAbove, 75, 75));
        assert!(realised_yes_for(ThresholdDirection::AtOrAbove, 75, 80));
        assert!(!realised_yes_for(ThresholdDirection::AtOrAbove, 75, 74));
    }

    #[test]
    fn realised_yes_at_or_below_uses_inclusive_bound() {
        assert!(realised_yes_for(ThresholdDirection::AtOrBelow, 30, 30));
        assert!(realised_yes_for(ThresholdDirection::AtOrBelow, 30, 25));
        assert!(!realised_yes_for(ThresholdDirection::AtOrBelow, 30, 31));
    }

    #[test]
    fn parse_decisions_text_round_trips_a_full_row() {
        let date = NaiveDate::from_ymd_opt(2026, 7, 4).unwrap();
        let r = row(
            TempStat::DailyHigh,
            ThresholdDirection::AtOrAbove,
            75,
            date,
            dec!(0.65),
            78,
        );
        // Use the same serde shape weather-bot writes; here we hand-roll a
        // matching JSON literal to assert we can read what they emit.
        let line = format!(
            r#"{{"ts":"{}","ticker":"X","city":"NY","settlement_station":"KNYC","stat":"daily_high","direction":"at_or_above","strike_temperature_f":75,"resolution_date":"2026-07-04","horizon_days":1,"forecast_temp_f":78,"sigma_f":2.0,"sigma_source":"static","model_p_yes":"0.65","yes_bid":null,"yes_ask":null,"spread":null,"decision":"trade","reason":null,"side":"yes","limit_price":null,"contracts":null,"fee_estimate":null,"raw_edge":null,"net_ev_per_contract":null,"market_p_implied":null}}"#,
            r.ts.to_rfc3339()
        );
        let parsed = parse_decisions_text(&line).unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].strike_temperature_f, 75);
        assert_eq!(parsed[0].stat, TempStat::DailyHigh);
        assert_eq!(parsed[0].direction, ThresholdDirection::AtOrAbove);
    }

    #[test]
    fn parse_decisions_text_skips_blank_lines_and_reports_line_number() {
        // Two blanks then a malformed row → the error should report line 4.
        let text = "\n\n\nnot json\n";
        let err = parse_decisions_text(text).unwrap_err();
        match err {
            BacktestError::Json { line_no, .. } => assert_eq!(line_no, 4),
            other => panic!("expected Json error, got {:?}", other),
        }
    }

    #[test]
    fn join_outcome_picks_high_for_high_market() {
        let date = NaiveDate::from_ymd_opt(2026, 7, 4).unwrap();
        let r = row(
            TempStat::DailyHigh,
            ThresholdDirection::AtOrAbove,
            75,
            date,
            dec!(0.65),
            78,
        );
        let cli_rows = vec![cli(date, Some(80), Some(60))];
        let j = join_outcome(&r, &cli_rows).unwrap();
        assert_eq!(j.realised_temp_f, 80);
        assert!(j.realised_yes);
    }

    #[test]
    fn join_outcome_picks_low_for_low_market() {
        let date = NaiveDate::from_ymd_opt(2026, 1, 5).unwrap();
        let r = row(
            TempStat::DailyLow,
            ThresholdDirection::AtOrBelow,
            30,
            date,
            dec!(0.6),
            28,
        );
        let cli_rows = vec![cli(date, Some(40), Some(28))];
        let j = join_outcome(&r, &cli_rows).unwrap();
        assert_eq!(j.realised_temp_f, 28);
        assert!(j.realised_yes);
    }

    #[test]
    fn join_outcome_returns_none_when_cli_missing() {
        let date = NaiveDate::from_ymd_opt(2026, 7, 4).unwrap();
        let r = row(
            TempStat::DailyHigh,
            ThresholdDirection::AtOrAbove,
            75,
            date,
            dec!(0.65),
            78,
        );
        // No CLI for that date.
        assert!(join_outcome(&r, &[]).is_none());
        // CLI for that date but the relevant extreme is missing.
        let cli_rows = vec![cli(date, None, Some(60))];
        assert!(join_outcome(&r, &cli_rows).is_none());
    }

    #[test]
    fn metrics_on_empty_input_zeroes_everything() {
        let m = metrics(&[]);
        assert_eq!(m.n, 0);
        assert_eq!(m.hit_rate, 0.0);
        assert_eq!(m.mean_brier, 0.0);
    }

    #[test]
    fn metrics_match_hand_computed_values_on_two_rows() {
        // Row 1: model_p = 0.8, realised YES, forecast 78 vs realised 80.
        // Row 2: model_p = 0.3, realised NO,  forecast 70 vs realised 65.
        let date = NaiveDate::from_ymd_opt(2026, 7, 4).unwrap();
        let r1 = row(
            TempStat::DailyHigh,
            ThresholdDirection::AtOrAbove,
            75,
            date,
            dec!(0.8),
            78,
        );
        let r2 = row(
            TempStat::DailyHigh,
            ThresholdDirection::AtOrAbove,
            75,
            date,
            dec!(0.3),
            70,
        );
        let j = vec![
            JoinedDecision {
                row: r1,
                realised_temp_f: 80,
                realised_yes: true,
            },
            JoinedDecision {
                row: r2,
                realised_temp_f: 65,
                realised_yes: false,
            },
        ];
        let m = metrics(&j);

        assert_eq!(m.n, 2);
        // Both rows: leaning matched outcome.
        assert!((m.hit_rate - 1.0).abs() < 1e-9);
        // Brier = ((0.8-1)^2 + (0.3-0)^2) / 2 = (0.04 + 0.09) / 2 = 0.065.
        assert!((m.mean_brier - 0.065).abs() < 1e-9);
        // Mean bias = ((78-80) + (70-65)) / 2 = ( -2 + 5 ) / 2 = 1.5.
        assert!((m.mean_temp_bias_f - 1.5).abs() < 1e-9);
        // log_loss ≈ -ln(0.8) for row 1, -ln(1-0.3)=-ln(0.7) for row 2.
        let expected = (-(0.8f64.ln()) + -(0.7f64.ln())) / 2.0;
        assert!((m.mean_log_loss - expected).abs() < 1e-6);
    }

    #[test]
    fn calibration_bucket_indexing_handles_edges() {
        // Build rows with model_p hitting bucket boundaries.
        let date = NaiveDate::from_ymd_opt(2026, 7, 4).unwrap();
        let mk = |p: Decimal, y: bool| JoinedDecision {
            row: row(
                TempStat::DailyHigh,
                ThresholdDirection::AtOrAbove,
                75,
                date,
                p,
                78,
            ),
            realised_temp_f: 80,
            realised_yes: y,
        };
        let j = vec![
            mk(dec!(0.0), false),  // bucket 0
            mk(dec!(0.05), false), // bucket 0
            mk(dec!(0.5), true),   // bucket 5
            mk(dec!(0.99), true),  // bucket 9
            mk(dec!(1.0), true),   // bucket 9 (clamped)
        ];
        let m = metrics(&j);
        assert_eq!(m.calibration[0].n, 2);
        assert_eq!(m.calibration[5].n, 1);
        assert_eq!(m.calibration[9].n, 2);
        // Empty middle buckets stay zeroed.
        assert_eq!(m.calibration[3].n, 0);
        assert_eq!(m.calibration[3].realised_yes_rate, 0.0);
    }

    #[test]
    fn series_ticker_derives_from_weather_market_ticker() {
        assert_eq!(
            series_ticker_from_market_ticker("KXHIGHNY-26MAY04-T70"),
            Some("KXHIGHNY")
        );
        assert_eq!(series_ticker_from_market_ticker("BAD"), None);
    }

    #[test]
    fn candle_containing_ts_uses_half_open_period() {
        let candle = candle(3_600, dec!(0.40), dec!(0.45));
        let inside = DateTime::from_timestamp(3_599, 0).unwrap();
        let boundary = DateTime::from_timestamp(3_600, 0).unwrap();
        assert!(candle_containing_ts(
            std::slice::from_ref(&candle),
            inside,
            PeriodInterval::OneHour
        )
        .is_some());
        assert!(candle_containing_ts(&[candle], boundary, PeriodInterval::OneHour).is_none());
    }

    #[test]
    fn estimate_trade_pnl_marks_yes_to_bid_close_net_of_fee() {
        let mut r = row(
            TempStat::DailyHigh,
            ThresholdDirection::AtOrAbove,
            75,
            NaiveDate::from_ymd_opt(2026, 7, 4).unwrap(),
            dec!(0.65),
            80,
        );
        r.decision = "trade".to_string();
        r.side = Some("yes".to_string());
        r.contracts = Some(10);
        r.limit_price = Some(dec!(0.45));
        r.fee_estimate = Some(dec!(0.02));

        let pnl = estimate_trade_pnl(&r, &candle(3_600, dec!(0.50), dec!(0.55))).unwrap();
        assert_eq!(pnl.mark_price, dec!(0.50));
        assert_eq!(pnl.gross_per_contract, dec!(0.05));
        assert_eq!(pnl.net_per_contract, dec!(0.03));
        assert_eq!(pnl.net_total, dec!(0.30));
    }

    #[test]
    fn estimate_trade_pnl_marks_no_to_implied_no_bid_close() {
        let mut r = row(
            TempStat::DailyHigh,
            ThresholdDirection::AtOrAbove,
            75,
            NaiveDate::from_ymd_opt(2026, 7, 4).unwrap(),
            dec!(0.35),
            80,
        );
        r.decision = "trade".to_string();
        r.side = Some("no".to_string());
        r.contracts = Some(5);
        r.limit_price = Some(dec!(0.45));
        r.fee_estimate = Some(dec!(0.01));

        let pnl = estimate_trade_pnl(&r, &candle(3_600, dec!(0.50), dec!(0.52))).unwrap();
        assert_eq!(pnl.mark_price, dec!(0.48));
        assert_eq!(pnl.net_per_contract, dec!(0.02));
        assert_eq!(pnl.net_total, dec!(0.10));
    }

    #[test]
    fn dedupe_joined_by_opportunity_keeps_earliest_per_ticker_and_date() {
        let date = NaiveDate::from_ymd_opt(2026, 7, 4).unwrap();
        let base = row(
            TempStat::DailyHigh,
            ThresholdDirection::AtOrAbove,
            75,
            date,
            dec!(0.65),
            78,
        );

        let mk = |ts_offset_secs: i64, ticker: &str, p: Decimal| {
            let mut r = base.clone();
            r.ts = base.ts + chrono::Duration::seconds(ts_offset_secs);
            r.ticker = ticker.to_string();
            r.model_p_yes = p;
            JoinedDecision {
                row: r,
                realised_temp_f: 80,
                realised_yes: true,
            }
        };

        let early_a = mk(0, "KXHIGHNY-26JUL04-T75", dec!(0.65));
        let later_a = mk(60, "KXHIGHNY-26JUL04-T75", dec!(0.70));
        let early_b = mk(30, "KXHIGHNY-26JUL04-T80", dec!(0.20));

        let deduped = dedupe_joined_by_opportunity(&[early_a, later_a, early_b]);
        assert_eq!(deduped.len(), 2);
        // First emission wins: A's preserved model_p is 0.65, not 0.70.
        let a = deduped
            .iter()
            .find(|j| j.row.ticker == "KXHIGHNY-26JUL04-T75")
            .unwrap();
        assert_eq!(a.row.model_p_yes, dec!(0.65));
    }

    #[test]
    fn dedupe_joined_by_opportunity_separates_by_resolution_date() {
        let mk = |date: NaiveDate| JoinedDecision {
            row: row(
                TempStat::DailyHigh,
                ThresholdDirection::AtOrAbove,
                75,
                date,
                dec!(0.65),
                78,
            ),
            realised_temp_f: 80,
            realised_yes: true,
        };
        let d1 = NaiveDate::from_ymd_opt(2026, 7, 4).unwrap();
        let d2 = NaiveDate::from_ymd_opt(2026, 7, 5).unwrap();
        let deduped = dedupe_joined_by_opportunity(&[mk(d1), mk(d2)]);
        assert_eq!(deduped.len(), 2);
    }

    #[test]
    fn filter_joined_to_trades_drops_no_trade_rows() {
        let date = NaiveDate::from_ymd_opt(2026, 7, 4).unwrap();
        let mut trade_row = JoinedDecision {
            row: row(
                TempStat::DailyHigh,
                ThresholdDirection::AtOrAbove,
                75,
                date,
                dec!(0.65),
                78,
            ),
            realised_temp_f: 80,
            realised_yes: true,
        };
        trade_row.row.decision = "trade".to_string();
        let mut no_trade_row = trade_row.clone();
        no_trade_row.row.decision = "no_trade".to_string();

        let filtered = filter_joined_to_trades(&[trade_row, no_trade_row]);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].row.decision, "trade");
    }

    #[test]
    fn metrics_by_sigma_source_splits_joined_rows() {
        let date = NaiveDate::from_ymd_opt(2026, 7, 4).unwrap();
        let mut static_row = JoinedDecision {
            row: row(
                TempStat::DailyHigh,
                ThresholdDirection::AtOrAbove,
                75,
                date,
                dec!(0.90),
                80,
            ),
            realised_temp_f: 80,
            realised_yes: true,
        };
        static_row.row.sigma_source = "static".to_string();
        let mut gefs_row = static_row.clone();
        gefs_row.row.sigma_source = "gefs_ensemble".to_string();

        let grouped = metrics_by_sigma_source(&[static_row, gefs_row]);
        assert_eq!(grouped["static"].n, 1);
        assert_eq!(grouped["gefs_ensemble"].n, 1);
    }

    fn candle(end_period_ts: i64, yes_bid_close: Decimal, yes_ask_close: Decimal) -> Candlestick {
        Candlestick {
            end_period_ts,
            price: None,
            yes_ask: weather_scanner::BookOhlc {
                open: yes_ask_close,
                high: yes_ask_close,
                low: yes_ask_close,
                close: yes_ask_close,
            },
            yes_bid: weather_scanner::BookOhlc {
                open: yes_bid_close,
                high: yes_bid_close,
                low: yes_bid_close,
                close: yes_bid_close,
            },
            volume_dollars: Decimal::ZERO,
            open_interest_dollars: Decimal::ZERO,
        }
    }
}
