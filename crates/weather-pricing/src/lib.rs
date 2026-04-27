//! Convert a NOAA NWS point forecast + a Kalshi `WeatherThreshold` into a
//! model probability that the YES contract resolves true.
//!
//! Model: assume actual temperature ~ Normal(forecast, sigma^2) where sigma
//! grows with forecast horizon (see `SIGMA_BY_HORIZON`). Apply a
//! half-degree continuity correction since NWS Climatological Reports — the
//! source of truth Kalshi resolves on — round to whole degrees Fahrenheit.
//!
//! Period matching:
//!   - DailyHigh on date X → NWS day period (`is_daytime=true`) with
//!     `start_time` calendar-day == X.
//!   - DailyLow on date X → NWS night period (`is_daytime=false`) with
//!     `start_time` calendar-day == X - 1. (The lowest temp during the
//!     calendar day of X almost always occurs in the early morning of X,
//!     which is covered by the NWS night period named "X-1 Night".)
//!
//! Sigma table is a starting estimate from NWS verification stats; calibrate
//! against observed errors once the bot has a track record.

use chrono::{Duration, NaiveDate};
use rust_decimal::Decimal;
use rust_decimal::prelude::FromPrimitive;
use thiserror::Error;
use weather_types::{Forecast, ForecastPeriod, TempStat, ThresholdDirection, WeatherThreshold};

#[derive(Debug, Error)]
pub enum PricingError {
    #[error("no forecast period matches threshold (date {date}, stat {stat:?})")]
    NoMatchingPeriod { date: NaiveDate, stat: TempStat },
    #[error("horizon {days} days exceeds NWS forecast range (max 6)")]
    HorizonTooFar { days: i64 },
    #[error("threshold date {date} is before forecast fetch date")]
    HorizonNegative { date: NaiveDate },
}

/// Standard deviation of forecast error (°F) by horizon-in-days. Index 0 is
/// "today's forecast issued today"; index 6 is the 7-day outlook. Values
/// approximate NWS aviation/temperature verification stats for major US
/// cities. Will be replaced with empirical calibration once we have a
/// realised-vs-forecast log.
const SIGMA_BY_HORIZON: [f64; 7] = [2.0, 2.5, 3.0, 3.5, 4.0, 4.5, 5.0];

/// Compute the model probability that a Kalshi YES contract on `threshold`
/// resolves true, given `forecast`.
pub fn model_probability(
    forecast: &Forecast,
    threshold: &WeatherThreshold,
) -> Result<Decimal, PricingError> {
    let horizon_days = (threshold.date - forecast.fetched_at.date_naive()).num_days();
    if horizon_days < 0 {
        return Err(PricingError::HorizonNegative {
            date: threshold.date,
        });
    }
    let sigma_idx = horizon_days as usize;
    if sigma_idx >= SIGMA_BY_HORIZON.len() {
        return Err(PricingError::HorizonTooFar {
            days: horizon_days,
        });
    }
    let sigma = SIGMA_BY_HORIZON[sigma_idx];

    let period = find_period(forecast, threshold).ok_or(PricingError::NoMatchingPeriod {
        date: threshold.date,
        stat: threshold.stat,
    })?;

    let forecast_t = period.temperature_f as f64;
    let threshold_t = threshold.temperature_f as f64;

    // Continuity correction: NWS rounds to whole degrees, so the half-degree
    // boundary is the right cutoff for the underlying continuous distribution.
    //   - "≥ T"  →  P(round(actual) ≥ T) ≈ P(actual ≥ T - 0.5)
    //                                    = Phi((forecast - (T - 0.5)) / sigma)
    //   - "≤ T"  →  P(round(actual) ≤ T) ≈ P(actual ≤ T + 0.5)
    //                                    = Phi(((T + 0.5) - forecast) / sigma)
    let z = match threshold.direction {
        ThresholdDirection::AtOrAbove => (forecast_t - threshold_t + 0.5) / sigma,
        ThresholdDirection::AtOrBelow => (threshold_t - forecast_t + 0.5) / sigma,
    };
    let p = normal_cdf(z).clamp(0.0, 1.0);

    Decimal::from_f64(p)
        .ok_or_else(|| PricingError::NoMatchingPeriod {
            date: threshold.date,
            stat: threshold.stat,
        })
}

fn find_period<'a>(
    forecast: &'a Forecast,
    threshold: &WeatherThreshold,
) -> Option<&'a ForecastPeriod> {
    match threshold.stat {
        TempStat::DailyHigh => forecast
            .periods
            .iter()
            .find(|p| p.is_daytime && p.start_time.date_naive() == threshold.date),
        TempStat::DailyLow => {
            let prev_day = threshold.date - Duration::days(1);
            forecast
                .periods
                .iter()
                .find(|p| !p.is_daytime && p.start_time.date_naive() == prev_day)
        }
    }
}

/// Standard normal CDF: Phi(z) = 0.5 * (1 + erf(z / sqrt(2))).
fn normal_cdf(z: f64) -> f64 {
    0.5 * (1.0 + libm::erf(z / std::f64::consts::SQRT_2))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};
    use weather_types::{Forecast, ForecastPeriod};

    fn dt(y: i32, m: u32, d: u32, h: u32) -> chrono::DateTime<Utc> {
        Utc.with_ymd_and_hms(y, m, d, h, 0, 0).unwrap()
    }

    fn day_period(date: NaiveDate, temp_f: i32) -> ForecastPeriod {
        ForecastPeriod {
            name: "Day".to_string(),
            is_daytime: true,
            temperature_f: temp_f,
            precipitation_probability_pct: None,
            start_time: dt(date.year(), date.month0() + 1, date.day(), 12),
            end_time: dt(date.year(), date.month0() + 1, date.day(), 23),
            detailed_forecast: String::new(),
        }
    }

    fn night_period(date: NaiveDate, temp_f: i32) -> ForecastPeriod {
        ForecastPeriod {
            name: "Night".to_string(),
            is_daytime: false,
            temperature_f: temp_f,
            precipitation_probability_pct: None,
            start_time: dt(date.year(), date.month0() + 1, date.day(), 18),
            end_time: dt(date.year(), date.month0() + 1, date.day(), 23) + Duration::hours(13),
            detailed_forecast: String::new(),
        }
    }

    use chrono::Datelike;

    fn forecast_for(periods: Vec<ForecastPeriod>, fetched: chrono::DateTime<Utc>) -> Forecast {
        Forecast {
            lat: 40.7790,
            lon: -73.9690,
            fetched_at: fetched,
            periods,
        }
    }

    fn high_threshold(date: NaiveDate, temp_f: i32, dir: ThresholdDirection) -> WeatherThreshold {
        WeatherThreshold {
            city: "NY".to_string(),
            stat: TempStat::DailyHigh,
            direction: dir,
            temperature_f: temp_f,
            date,
        }
    }

    fn d(s: &str) -> Decimal {
        s.parse().unwrap()
    }

    #[test]
    fn at_or_above_when_forecast_well_above_threshold_is_near_one() {
        // Forecast=80, threshold=70 (≥), sigma=2 (today). Phi(5.25) ≈ 1.
        let date = NaiveDate::from_ymd_opt(2026, 7, 4).unwrap();
        let f = forecast_for(vec![day_period(date, 80)], dt(2026, 7, 4, 8));
        let t = high_threshold(date, 70, ThresholdDirection::AtOrAbove);
        let p = model_probability(&f, &t).unwrap();
        assert!(p > d("0.99"), "expected ~1.0, got {}", p);
    }

    #[test]
    fn at_or_above_when_forecast_well_below_threshold_is_near_zero() {
        // Forecast=60, threshold=70 (≥), sigma=2. Phi(-4.75) ≈ 0.
        let date = NaiveDate::from_ymd_opt(2026, 7, 4).unwrap();
        let f = forecast_for(vec![day_period(date, 60)], dt(2026, 7, 4, 8));
        let t = high_threshold(date, 70, ThresholdDirection::AtOrAbove);
        let p = model_probability(&f, &t).unwrap();
        assert!(p < d("0.01"), "expected ~0.0, got {}", p);
    }

    #[test]
    fn at_or_above_at_threshold_is_above_half_due_to_continuity() {
        // Forecast=70, threshold=70 (≥), sigma=2.
        // Phi((70 - 70 + 0.5)/2) = Phi(0.25) ≈ 0.5987.
        // Without continuity correction this would be 0.5; the half-degree
        // bump captures the "round to nearest integer" rule.
        let date = NaiveDate::from_ymd_opt(2026, 7, 4).unwrap();
        let f = forecast_for(vec![day_period(date, 70)], dt(2026, 7, 4, 8));
        let t = high_threshold(date, 70, ThresholdDirection::AtOrAbove);
        let p = model_probability(&f, &t).unwrap();
        assert!(p > d("0.59") && p < d("0.61"), "expected ≈0.5987, got {}", p);
    }

    #[test]
    fn at_or_below_at_threshold_is_above_half_due_to_continuity() {
        // Symmetric to above. Phi(0.25) ≈ 0.5987.
        let date = NaiveDate::from_ymd_opt(2026, 7, 4).unwrap();
        let f = forecast_for(vec![day_period(date, 70)], dt(2026, 7, 4, 8));
        let t = high_threshold(date, 70, ThresholdDirection::AtOrBelow);
        let p = model_probability(&f, &t).unwrap();
        assert!(p > d("0.59") && p < d("0.61"), "expected ≈0.5987, got {}", p);
    }

    #[test]
    fn sigma_grows_with_horizon() {
        // Same forecast value vs threshold at horizons 0 and 6 — at horizon
        // 0 (sigma=2) the probability should be more confident (further
        // from 0.5) than at horizon 6 (sigma=5).
        let near_date = NaiveDate::from_ymd_opt(2026, 7, 4).unwrap();
        let far_date = NaiveDate::from_ymd_opt(2026, 7, 10).unwrap();
        let f_near = forecast_for(vec![day_period(near_date, 75)], dt(2026, 7, 4, 8));
        let f_far = forecast_for(vec![day_period(far_date, 75)], dt(2026, 7, 4, 8));
        let t_near = high_threshold(near_date, 70, ThresholdDirection::AtOrAbove);
        let t_far = high_threshold(far_date, 70, ThresholdDirection::AtOrAbove);
        let p_near = model_probability(&f_near, &t_near).unwrap();
        let p_far = model_probability(&f_far, &t_far).unwrap();
        assert!(
            p_near > p_far,
            "horizon 0 should be more confident than horizon 6 (got {} vs {})",
            p_near,
            p_far
        );
    }

    #[test]
    fn daily_low_uses_previous_night_period() {
        // Threshold: low for July 4. Should match the night period of July 3
        // (which covers July 3 evening → July 4 early morning, where the
        // overnight low typically occurs).
        let target_date = NaiveDate::from_ymd_opt(2026, 7, 4).unwrap();
        let prev_night = NaiveDate::from_ymd_opt(2026, 7, 3).unwrap();
        let f = forecast_for(
            vec![
                day_period(prev_night, 90),       // July 3 day, irrelevant
                night_period(prev_night, 65),     // ← the right one for July 4 low
                day_period(target_date, 92),      // July 4 day, irrelevant
            ],
            dt(2026, 7, 3, 8),
        );
        let threshold = WeatherThreshold {
            city: "NY".to_string(),
            stat: TempStat::DailyLow,
            direction: ThresholdDirection::AtOrAbove,
            temperature_f: 60,
            date: target_date,
        };
        let p = model_probability(&f, &threshold).unwrap();
        // forecast 65, threshold ≥ 60, sigma=1 day = 2.5.
        // Phi((65 - 60 + 0.5)/2.5) = Phi(2.2) ≈ 0.9861.
        assert!(p > d("0.97") && p < d("1.0"), "expected ≈0.986, got {}", p);
    }

    #[test]
    fn missing_matching_period_errors() {
        let date = NaiveDate::from_ymd_opt(2026, 7, 4).unwrap();
        let other = NaiveDate::from_ymd_opt(2026, 7, 6).unwrap();
        // Forecast only has periods for date+2; nothing for date itself.
        let f = forecast_for(vec![day_period(other, 80)], dt(2026, 7, 4, 8));
        let t = high_threshold(date, 70, ThresholdDirection::AtOrAbove);
        assert!(matches!(
            model_probability(&f, &t),
            Err(PricingError::NoMatchingPeriod { .. })
        ));
    }

    #[test]
    fn horizon_too_far_errors() {
        let near = NaiveDate::from_ymd_opt(2026, 7, 4).unwrap();
        let way_out = NaiveDate::from_ymd_opt(2026, 7, 20).unwrap();
        let f = forecast_for(vec![day_period(way_out, 80)], dt(2026, 7, 4, 8));
        let t = high_threshold(way_out, 70, ThresholdDirection::AtOrAbove);
        assert!(matches!(
            model_probability(&f, &t),
            Err(PricingError::HorizonTooFar { days: 16 })
        ));
        let _ = near;
    }

    #[test]
    fn negative_horizon_errors() {
        // Threshold date before forecast fetch → market already past.
        let past = NaiveDate::from_ymd_opt(2026, 7, 1).unwrap();
        let f = forecast_for(vec![], dt(2026, 7, 4, 8));
        let t = high_threshold(past, 70, ThresholdDirection::AtOrAbove);
        assert!(matches!(
            model_probability(&f, &t),
            Err(PricingError::HorizonNegative { .. })
        ));
    }
}
