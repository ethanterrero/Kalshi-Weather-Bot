//! Historical (Phase A) calibration: synthesize decisions over an archived
//! forecast × CLI realised pair, then run the standard `metrics()` over
//! the result. This is the gate that says "the model's `P(YES)` is
//! well-calibrated against ground truth on past data" *before* committing
//! to a real-time dry-run.
//!
//! The math intentionally does NOT route through `weather-pricing::price_market`
//! — that takes a live `Forecast` with day/night `ForecastPeriod` slots,
//! which the historical archive doesn't naturally produce. Instead we
//! compute the model probability directly from the (daily-high or daily-
//! low) forecast value via `weather-pricing::normal_cdf`. This keeps the
//! Normal-CDF + continuity-correction formula identical to what
//! `price_market` uses, just bypassing the period-window machinery.
//!
//! Same-day horizon only in v1. Multi-horizon backtesting needs a forecast
//! source indexed by issue time, which Open-Meteo's historical-forecast
//! archive doesn't expose directly.

use chrono::{NaiveDate, Utc};
use rust_decimal::prelude::FromPrimitive;
use rust_decimal::Decimal;

use weather_forecast::{CliReport, HistoricalForecast};
use weather_pricing::normal_cdf;
use weather_types::{CitySpec, TempStat, ThresholdDirection};

use crate::{realised_yes_for, DecisionRow, JoinedDecision};

/// What to evaluate. The decision space is the cartesian product of
/// `dates × {high_strikes_f x AtOrAbove, AtOrBelow} × {low_strikes_f x
/// AtOrAbove, AtOrBelow}`. Strikes outside the realistic range for that
/// city/season produce trivially calibrated rows (model_p ≈ 0 or 1) and
/// drown out the interesting middle — keep the strike list tight.
#[derive(Debug, Clone)]
pub struct CalibrationPlan {
    pub dates: Vec<NaiveDate>,
    pub high_strikes_f: Vec<i32>,
    pub low_strikes_f: Vec<i32>,
    /// σ to feed the Normal-CDF. Day-0 forecast errors (`sigma_for_horizon(0)`)
    /// is the natural default since the historical archive is same-day.
    pub sigma_f: f64,
}

/// Build a vector of `JoinedDecision`s by walking `plan` against the
/// archived `forecast` and `cli_reports`. Rows are skipped silently when:
///   - the daily extreme can't be derived from the archive (date past edge)
///   - no CLI report matches the date
///   - the CLI's high/low for the stat is `None` (M-flagged in IEM)
pub fn historical_calibration(
    city: &CitySpec,
    forecast: &HistoricalForecast,
    cli_reports: &[CliReport],
    plan: &CalibrationPlan,
) -> Vec<JoinedDecision> {
    let mut out = Vec::new();
    for &date in &plan.dates {
        let cli = cli_reports.iter().find(|r| r.date == date);
        let Some(cli) = cli else { continue };

        // High side.
        if let Some(forecast_high) = forecast.daily_high(city, date) {
            if let Some(realised_high) = cli.high_f {
                for &strike in &plan.high_strikes_f {
                    for direction in [ThresholdDirection::AtOrAbove, ThresholdDirection::AtOrBelow]
                    {
                        out.push(synth_decision(
                            city,
                            date,
                            TempStat::DailyHigh,
                            direction,
                            strike,
                            forecast_high,
                            realised_high,
                            plan.sigma_f,
                        ));
                    }
                }
            }
        }

        // Low side.
        if let Some(forecast_low) = forecast.daily_low(city, date) {
            if let Some(realised_low) = cli.low_f {
                for &strike in &plan.low_strikes_f {
                    for direction in [ThresholdDirection::AtOrAbove, ThresholdDirection::AtOrBelow]
                    {
                        out.push(synth_decision(
                            city,
                            date,
                            TempStat::DailyLow,
                            direction,
                            strike,
                            forecast_low,
                            realised_low,
                            plan.sigma_f,
                        ));
                    }
                }
            }
        }
    }
    out
}

/// Compute model `P(YES)` from a forecast value via the same Normal-CDF +
/// continuity-correction formula `weather-pricing::yes_probability` uses,
/// without going through the period-window machinery.
pub fn model_p_yes(
    forecast_temp_f: i32,
    strike_temperature_f: i32,
    direction: ThresholdDirection,
    sigma_f: f64,
) -> f64 {
    let mu = forecast_temp_f as f64;
    let t = strike_temperature_f as f64;
    let p = match direction {
        ThresholdDirection::AtOrAbove => 1.0 - normal_cdf((t - 0.5 - mu) / sigma_f),
        ThresholdDirection::AtOrBelow => normal_cdf((t + 0.5 - mu) / sigma_f),
    };
    p.clamp(0.0, 1.0)
}

#[allow(clippy::too_many_arguments)]
fn synth_decision(
    city: &CitySpec,
    date: NaiveDate,
    stat: TempStat,
    direction: ThresholdDirection,
    strike: i32,
    forecast_temp: i32,
    realised_temp: i32,
    sigma_f: f64,
) -> JoinedDecision {
    let p = model_p_yes(forecast_temp, strike, direction, sigma_f);
    let p_dec = Decimal::from_f64(p).unwrap_or(Decimal::ZERO);
    let realised_yes = realised_yes_for(direction, strike, realised_temp);

    let row = DecisionRow {
        ts: Utc::now(),
        ticker: format!(
            "SYNTHETIC-{}-{}-{}-T{}",
            city.kalshi_code,
            date.format("%y%m%d"),
            match stat {
                TempStat::DailyHigh => "HIGH",
                TempStat::DailyLow => "LOW",
            },
            strike
        ),
        city: city.kalshi_code.to_string(),
        settlement_station: city.icao.to_string(),
        stat,
        direction,
        strike_temperature_f: strike,
        resolution_date: date,
        horizon_days: 0,
        forecast_temp_f: forecast_temp,
        sigma_f,
        model_p_yes: p_dec,
        yes_bid: None,
        yes_ask: None,
        spread: None,
        decision: "synthetic".to_string(),
        reason: None,
        side: None,
        limit_price: None,
        contracts: None,
        fee_estimate: None,
        raw_edge: None,
        net_ev_per_contract: None,
        market_p_implied: None,
    };
    JoinedDecision {
        row,
        realised_temp_f: realised_temp,
        realised_yes,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics;
    use weather_forecast::parse_historical_json;
    use weather_types::lookup_city;

    /// Same fixture shape as the historical_forecast unit tests: 24h
    /// peaking at 90°F mid-afternoon, NYC standard-time.
    const FIXTURE_24H: &str = r#"{
        "hourly": {
            "time": [
                "2026-07-04T05:00", "2026-07-04T06:00", "2026-07-04T07:00", "2026-07-04T08:00",
                "2026-07-04T09:00", "2026-07-04T10:00", "2026-07-04T11:00", "2026-07-04T12:00",
                "2026-07-04T13:00", "2026-07-04T14:00", "2026-07-04T15:00", "2026-07-04T16:00",
                "2026-07-04T17:00", "2026-07-04T18:00", "2026-07-04T19:00", "2026-07-04T20:00",
                "2026-07-04T21:00", "2026-07-04T22:00", "2026-07-04T23:00", "2026-07-05T00:00",
                "2026-07-05T01:00", "2026-07-05T02:00", "2026-07-05T03:00", "2026-07-05T04:00"
            ],
            "temperature_2m": [
                70.1, 69.4, 68.6, 70.0, 73.2, 76.5, 79.4, 82.1,
                84.6, 86.7, 88.0, 89.5, 90.0, 89.6, 88.0, 86.0,
                83.0, 79.5, 76.0, 73.0, 71.5, 70.4, 70.0, 69.8
            ]
        }
    }"#;

    fn july_4() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 7, 4).unwrap()
    }

    fn nyc_cli(date: NaiveDate, high: Option<i32>, low: Option<i32>) -> CliReport {
        CliReport {
            station: "KNYC".into(),
            date,
            high_f: high,
            low_f: low,
        }
    }

    #[test]
    fn model_p_at_threshold_is_near_50_50_with_continuity_correction() {
        // Forecast 75, strike 75. Continuity correction makes the answer
        // slightly above 50% for AtOrAbove (we round toward inclusion of
        // the boundary), and slightly above 50% for AtOrBelow too — they
        // sum to 1 + a small overlap from the ±0.5 fudge.
        let p_above = model_p_yes(75, 75, ThresholdDirection::AtOrAbove, 1.6);
        let p_below = model_p_yes(75, 75, ThresholdDirection::AtOrBelow, 1.6);
        assert!(p_above > 0.5 && p_above < 0.7, "got {}", p_above);
        assert!(p_below > 0.5 && p_below < 0.7, "got {}", p_below);
    }

    #[test]
    fn model_p_far_above_threshold_is_near_one_for_at_or_above() {
        let p = model_p_yes(95, 75, ThresholdDirection::AtOrAbove, 1.6);
        assert!(p > 0.99, "got {}", p);
    }

    #[test]
    fn model_p_far_below_threshold_is_near_zero_for_at_or_above() {
        let p = model_p_yes(60, 75, ThresholdDirection::AtOrAbove, 1.6);
        assert!(p < 0.01, "got {}", p);
    }

    #[test]
    fn synth_decision_realised_yes_lines_up_with_strike_and_direction() {
        let nyc = lookup_city("NY").unwrap();
        // Forecast 88, realised 92, strike 90, AtOrAbove → realised YES.
        let j = synth_decision(
            nyc,
            july_4(),
            TempStat::DailyHigh,
            ThresholdDirection::AtOrAbove,
            90,
            88,
            92,
            1.6,
        );
        assert_eq!(j.realised_temp_f, 92);
        assert!(j.realised_yes);
        assert_eq!(j.row.stat, TempStat::DailyHigh);
        assert_eq!(j.row.direction, ThresholdDirection::AtOrAbove);
        assert_eq!(j.row.forecast_temp_f, 88);
        assert_eq!(j.row.strike_temperature_f, 90);
        assert_eq!(j.row.horizon_days, 0);
        assert_eq!(j.row.settlement_station, "KNYC");
        assert_eq!(j.row.decision, "synthetic");
    }

    #[test]
    fn historical_calibration_walks_strike_grid() {
        let nyc = lookup_city("NY").unwrap();
        let f = parse_historical_json(FIXTURE_24H).unwrap();
        let cli = vec![nyc_cli(july_4(), Some(91), Some(70))];
        let plan = CalibrationPlan {
            dates: vec![july_4()],
            high_strikes_f: vec![85, 90, 95],
            low_strikes_f: vec![68, 72],
            sigma_f: 1.6,
        };

        let joined = historical_calibration(nyc, &f, &cli, &plan);
        // 1 date × (3 high strikes × 2 directions + 2 low strikes × 2 directions)
        //   = 1 × (6 + 4) = 10 rows.
        assert_eq!(joined.len(), 10);
        // Spot-check one: HIGH 90 AtOrAbove, realised 91 → YES.
        let pick = joined
            .iter()
            .find(|j| {
                j.row.stat == TempStat::DailyHigh
                    && j.row.direction == ThresholdDirection::AtOrAbove
                    && j.row.strike_temperature_f == 90
            })
            .unwrap();
        assert!(pick.realised_yes);
    }

    #[test]
    fn historical_calibration_skips_dates_with_no_cli() {
        let nyc = lookup_city("NY").unwrap();
        let f = parse_historical_json(FIXTURE_24H).unwrap();
        // Empty CLI → no joins.
        let plan = CalibrationPlan {
            dates: vec![july_4()],
            high_strikes_f: vec![85],
            low_strikes_f: vec![],
            sigma_f: 1.6,
        };
        assert!(historical_calibration(nyc, &f, &[], &plan).is_empty());
    }

    #[test]
    fn historical_calibration_skips_when_cli_missing_extreme() {
        let nyc = lookup_city("NY").unwrap();
        let f = parse_historical_json(FIXTURE_24H).unwrap();
        // CLI has high but not low → low rows skipped.
        let cli = vec![nyc_cli(july_4(), Some(91), None)];
        let plan = CalibrationPlan {
            dates: vec![july_4()],
            high_strikes_f: vec![85],
            low_strikes_f: vec![70], // would-be 2 rows, all skipped
            sigma_f: 1.6,
        };
        let joined = historical_calibration(nyc, &f, &cli, &plan);
        // Only the high side fires: 1 strike × 2 directions = 2 rows.
        assert_eq!(joined.len(), 2);
        for j in &joined {
            assert_eq!(j.row.stat, TempStat::DailyHigh);
        }
    }

    #[test]
    fn metrics_on_calibrated_synthetic_data_are_well_behaved() {
        let nyc = lookup_city("NY").unwrap();
        let f = parse_historical_json(FIXTURE_24H).unwrap();
        // Forecast says 90; realised is 90. Strike grid bracketing: extreme
        // strikes should resolve trivially right; strikes near the forecast
        // are the calibration test.
        let cli = vec![nyc_cli(july_4(), Some(90), Some(70))];
        let plan = CalibrationPlan {
            dates: vec![july_4()],
            high_strikes_f: vec![70, 80, 90, 95],
            low_strikes_f: vec![],
            sigma_f: 1.6,
        };
        let joined = historical_calibration(nyc, &f, &cli, &plan);
        let m = metrics(&joined);
        // 4 strikes × 2 directions = 8 rows.
        assert_eq!(m.n, 8);
        // Forecast bias = 90 - 90 = 0.
        assert!((m.mean_temp_bias_f).abs() < 1e-9);
        // Brier should be small — the model's prediction of YES rate per
        // strike is broadly correct because the forecast equals the
        // realised value.
        assert!(
            m.mean_brier < 0.20,
            "Brier on perfect-forecast synthetic data should be small; got {}",
            m.mean_brier
        );
    }
}
