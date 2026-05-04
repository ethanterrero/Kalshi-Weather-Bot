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

pub use historical::{historical_calibration, model_p_yes, CalibrationPlan};

use chrono::{DateTime, NaiveDate, Utc};
use rust_decimal::Decimal;
use serde::Deserialize;
use std::path::Path;
use thiserror::Error;

use weather_forecast::CliReport;
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
    use rust_decimal::prelude::ToPrimitive;
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
}
