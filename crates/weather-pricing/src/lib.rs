//! Probabilistic pricing of weather threshold markets.
//!
//! Given a NOAA NWS point forecast for a city + day and a Kalshi
//! `WeatherThreshold` market on that same day, produce a model probability
//! that the YES side resolves true.
//!
//! v1 model — Normal forecast errors:
//!     P(YES | "≥ T") = 1 - Φ((T - μ) / σ) = Φ((μ - T) / σ)
//!     P(YES | "≤ T") = Φ((T - μ) / σ)
//! where μ is the forecast point estimate (°F) for the appropriate day-or-night
//! NWS period intersecting the Kalshi standard-time settlement window, and σ
//! is a horizon-indexed scale derived from published NWS verification stats.
//!
//! Why a distribution instead of a deterministic threshold check: the NWS
//! point forecast is a single number, but the realized high temperature has
//! noise. Sitting on the threshold (μ ≈ T) is *not* edge — the market should
//! price ~50/50 there too. Edge appears when the distribution's mass sits
//! clearly above or below the threshold in a way that disagrees with the
//! market price. A deterministic "forecast > T → buy YES" overrates obvious
//! cases the market already prices, and underrates close cases where the
//! distribution's tail does the work.
//!
//! Calibration is intentionally crude here — published NWS verification
//! shows day-1 high-temp MAE around 2–3°F nationally, growing roughly
//! linearly to 5–6°F at day 7. We use σ in (1.6°F → 5.0°F) keyed on the
//! horizon in days. Future work: per-city, per-season σ, and an ensemble
//! source (GEFS / ECMWF open data) to replace the fixed σ entirely.

use rust_decimal::prelude::FromPrimitive;
use rust_decimal::Decimal;

use weather_types::{
    daily_high_window_utc, daily_low_window_utc, lookup_city, Forecast, ForecastPeriod, TempStat,
    ThresholdDirection, WeatherThreshold,
};

/// Why we couldn't model a market. These are surfaced in dry-run logs so the
/// reason for abstaining is visible.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PricingError {
    /// City code (e.g. "NY") isn't in the cities table — we don't know which
    /// NWS station Kalshi settles on, so refuse to model it.
    UnknownCity(String),
    /// No forecast period matches the standard-time settlement window. This
    /// happens when the market's date is outside NWS's 7-day horizon, or the
    /// forecast has been refreshed past the market.
    NoMatchingForecastPeriod,
}

impl std::fmt::Display for PricingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PricingError::UnknownCity(c) => {
                write!(f, "no NWS station mapped for Kalshi city code '{}'", c)
            }
            PricingError::NoMatchingForecastPeriod => write!(
                f,
                "no NWS forecast period overlaps the Kalshi settlement window"
            ),
        }
    }
}

impl std::error::Error for PricingError {}

/// A pricing result includes both the probability and the inputs used so the
/// strategy/log layer can show *why* a probability came out the way it did.
#[derive(Debug, Clone)]
pub struct ModelPricing {
    /// Model probability YES resolves true.
    pub yes_probability: Decimal,
    /// Forecast temp used (°F).
    pub forecast_temp_f: i32,
    /// σ used (°F) — horizon-indexed.
    pub sigma_f: f64,
    /// Days from forecast `fetched_at` to the market date — the horizon used
    /// to look up σ.
    pub horizon_days: i64,
    /// ICAO of the NWS station Kalshi settles on, validated against the
    /// city table. Surfaced so logs show a settlement-source check.
    pub settlement_station: &'static str,
    /// Where σ came from. One of [`SIGMA_SOURCE_STATIC`],
    /// [`SIGMA_SOURCE_GEFS_ENSEMBLE`], [`SIGMA_SOURCE_ECMWF_ENSEMBLE`],
    /// or [`SIGMA_SOURCE_GEFS_ECMWF_BLEND`] (pooled members from both
    /// ensembles). The JSONL backtester splits metrics on this so we can
    /// tell which source moves the calibration needle.
    pub sigma_source: &'static str,
}

/// Source tags for [`ModelPricing::sigma_source`]. Kept as constants
/// rather than an enum so the field stays a `&'static str` (matches
/// `settlement_station`) — the JSONL layer just stringifies it.
pub const SIGMA_SOURCE_STATIC: &str = "static";
pub const SIGMA_SOURCE_GEFS_ENSEMBLE: &str = "gefs_ensemble";
pub const SIGMA_SOURCE_ECMWF_ENSEMBLE: &str = "ecmwf_ensemble";
pub const SIGMA_SOURCE_GEFS_ECMWF_BLEND: &str = "gefs_ecmwf_blend";
/// Special "source" tag: the threshold was locked by an in-window METAR
/// observation, so the model probability is derived from a realised
/// extreme rather than a forecast distribution. `sigma_f` is `0.0` for
/// these rows — the lock is monotone, so there is no remaining noise
/// for σ to describe.
pub const SIGMA_SOURCE_METAR_LOCK: &str = "metar_lock";

/// Probability we stamp on a locked YES — slightly below 1.0 to leave
/// a sliver of room for late QC revision or a settlement-rule edge case.
/// Tight enough that the strategy gate trades it aggressively; not 1.0
/// so the strategy code path stays inside the [0.0, 1.0) numerical band
/// it already trusts.
pub const METAR_LOCK_YES_PROBABILITY: f64 = 0.99;

/// Compute a model probability for a single Kalshi weather market.
///
/// Sanity-anchor: when the NWS forecast for the market's day equals the
/// threshold exactly, P(YES | "≥ T") sits right around 0.5 — the model
/// shouldn't pretend "exactly on the strike" is an edge.
///
/// ```
/// use chrono::{DateTime, NaiveDate, Utc};
/// use weather_pricing::price_market;
/// use weather_types::{Forecast, ForecastPeriod, TempStat, ThresholdDirection, WeatherThreshold};
///
/// fn dt(s: &str) -> DateTime<Utc> {
///     DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
/// }
/// let forecast = Forecast {
///     lat: 40.78, lon: -73.97,
///     fetched_at: dt("2026-07-03T12:00:00Z"),
///     generated_at: None,
///     periods: vec![ForecastPeriod {
///         name: "Saturday".into(),
///         is_daytime: true,
///         temperature_f: 75,
///         precipitation_probability_pct: None,
///         start_time: dt("2026-07-04T10:00:00Z"),
///         end_time: dt("2026-07-04T22:00:00Z"),
///         detailed_forecast: "".into(),
///     }],
/// };
/// let threshold = WeatherThreshold {
///     city: "NY".into(),
///     stat: TempStat::DailyHigh,
///     direction: ThresholdDirection::AtOrAbove,
///     temperature_f: 75,
///     date: NaiveDate::from_ymd_opt(2026, 7, 4).unwrap(),
/// };
/// let p = price_market(&threshold, &forecast).unwrap();
/// let pf: f64 = p.yes_probability.try_into().unwrap();
/// // Forecast equals threshold ⇒ P(YES) ≈ 0.5 (slightly higher because
/// // continuity correction shifts mass into the ≥-side bucket).
/// assert!(pf > 0.5 && pf < 0.7, "got {}", pf);
/// ```
pub fn price_market(
    threshold: &WeatherThreshold,
    forecast: &Forecast,
) -> Result<ModelPricing, PricingError> {
    price_market_with_sigma(threshold, forecast, None)
}

/// Same as [`price_market`], but lets the caller override the σ used in
/// the Normal-CDF. When `sigma_override` is `None` we fall back to the
/// hand-calibrated `sigma_for_horizon(horizon_days)` table — that's the
/// historical default, kept for backwards compatibility.
///
/// The override is a `(sigma_f, source_tag)` tuple: the caller is
/// authoritative about *which* ensemble (or blend) it computed the σ
/// from, and the tag flows through to `ModelPricing::sigma_source` and
/// the JSONL log. Pricing math is otherwise identical: same window
/// selection, same continuity correction, same period-overlap rule.
pub fn price_market_with_sigma(
    threshold: &WeatherThreshold,
    forecast: &Forecast,
    sigma_override: Option<(f64, &'static str)>,
) -> Result<ModelPricing, PricingError> {
    let city = lookup_city(&threshold.city)
        .ok_or_else(|| PricingError::UnknownCity(threshold.city.clone()))?;

    let (window_start, window_end) = match threshold.stat {
        TempStat::DailyHigh => daily_high_window_utc(city, threshold.date),
        TempStat::DailyLow => daily_low_window_utc(city, threshold.date),
    };

    // Pick the period whose midpoint lies inside the standard-time settlement
    // window AND whose day/night flag matches the stat. A high-temp market
    // matches the daytime period overlapping the window, low matches the
    // nighttime period.
    let want_daytime = matches!(threshold.stat, TempStat::DailyHigh);
    let chosen = forecast
        .periods
        .iter()
        .filter(|p| p.is_daytime == want_daytime)
        .find(|p| period_overlaps_window(p, window_start, window_end))
        .ok_or(PricingError::NoMatchingForecastPeriod)?;

    let horizon_days = (threshold.date - forecast.fetched_at.date_naive())
        .num_days()
        .max(0);
    let (sigma_f, sigma_source) = match sigma_override {
        Some((s, tag)) => (s, tag),
        None => (sigma_for_horizon(horizon_days), SIGMA_SOURCE_STATIC),
    };
    let yes_p = yes_probability(chosen.temperature_f, threshold, sigma_f);

    Ok(ModelPricing {
        yes_probability: Decimal::from_f64(yes_p).unwrap_or(Decimal::ZERO),
        forecast_temp_f: chosen.temperature_f,
        sigma_f,
        horizon_days,
        settlement_station: city.icao,
        sigma_source,
    })
}

/// Override pricing when the threshold has been *locked* by an in-window
/// METAR observation. Returns `Some(ModelPricing)` only when the realised
/// extreme already crossed the strike — the call site falls through to
/// the ensemble-based pricing otherwise.
///
/// Lock conditions (with continuity correction; Kalshi rounds to whole °F):
///   - `DailyHigh` + `AtOrAbove(T)` → running high ≥ T - 0.5 → YES locked.
///     CLI reports the day's max, which is monotone increasing; once a
///     valid observation has crossed T (rounded), the daily max is at
///     least T regardless of what happens later in the window.
///   - `DailyLow` + `AtOrBelow(T)` → running low ≤ T + 0.5 → YES locked.
///     Same monotonicity argument on the daily min.
///
/// We deliberately do **not** lock the opposite directions here
/// (`DailyHigh + AtOrBelow`, `DailyLow + AtOrAbove`). Those are "NO
/// locks": the YES side is becoming less likely, but you'd need the rest
/// of the day's forecast to know how much. That's a follow-up — this PR
/// ships only the high-confidence YES side.
///
/// `sigma_f` is set to `0.0` and `sigma_source` to
/// [`SIGMA_SOURCE_METAR_LOCK`] so backtest splits separate locked rows
/// from forecast-driven rows. The probability stamped is
/// [`METAR_LOCK_YES_PROBABILITY`] (slightly under 1.0) — see that
/// constant for the rationale.
pub fn price_market_with_lock(
    threshold: &WeatherThreshold,
    running_extreme_f: f64,
) -> Result<Option<ModelPricing>, PricingError> {
    let city = lookup_city(&threshold.city)
        .ok_or_else(|| PricingError::UnknownCity(threshold.city.clone()))?;
    let t = threshold.temperature_f as f64;
    let locked = match (threshold.stat, threshold.direction) {
        // "high ≥ T" locks once a valid running high crosses T - 0.5
        // (Kalshi rounding turns observed 70.0 into reported 70 ≥ 70 ✓).
        (TempStat::DailyHigh, ThresholdDirection::AtOrAbove) => running_extreme_f >= t - 0.5,
        // "low ≤ T" locks once a valid running low dips to T + 0.5 or below.
        (TempStat::DailyLow, ThresholdDirection::AtOrBelow) => running_extreme_f <= t + 0.5,
        // NO-side locks deferred.
        _ => false,
    };
    if !locked {
        return Ok(None);
    }
    Ok(Some(ModelPricing {
        yes_probability: Decimal::from_f64(METAR_LOCK_YES_PROBABILITY).unwrap_or(Decimal::ZERO),
        forecast_temp_f: running_extreme_f.round() as i32,
        sigma_f: 0.0,
        horizon_days: 0,
        settlement_station: city.icao,
        sigma_source: SIGMA_SOURCE_METAR_LOCK,
    }))
}

/// Does forecast period `p` overlap the half-open settlement window?
fn period_overlaps_window(
    p: &ForecastPeriod,
    window_start: chrono::DateTime<chrono::Utc>,
    window_end: chrono::DateTime<chrono::Utc>,
) -> bool {
    p.start_time < window_end && p.end_time > window_start
}

/// Standard deviation of the NWS forecast error, as a function of horizon in
/// days. Calibrated by hand from publicly reported NWS day-1..day-7 verifs.
/// Day 0 / day 1 ≈ 2°F, growing to ~5°F at day 7, capped at 6°F beyond.
///
/// ```
/// use weather_pricing::sigma_for_horizon;
/// // σ widens monotonically with horizon — a day-6 forecast is noisier
/// // than a day-1 forecast, and that's load-bearing for the EV gate.
/// assert!(sigma_for_horizon(1) < sigma_for_horizon(3));
/// assert!(sigma_for_horizon(3) < sigma_for_horizon(7));
/// // Beyond 7 days we cap at 6°F rather than extrapolating linearly into
/// // garbage; the static table shouldn't pretend day-30 is precise.
/// assert!((sigma_for_horizon(30) - 6.0).abs() < 1e-9);
/// ```
pub fn sigma_for_horizon(horizon_days: i64) -> f64 {
    match horizon_days {
        0 => 1.6,
        1 => 2.0,
        2 => 2.6,
        3 => 3.2,
        4 => 3.8,
        5 => 4.3,
        6 => 4.7,
        7 => 5.0,
        _ => 6.0,
    }
}

/// Probability YES resolves true under a Normal(μ, σ²) forecast-error model.
fn yes_probability(forecast_mu: i32, threshold: &WeatherThreshold, sigma: f64) -> f64 {
    let mu = forecast_mu as f64;
    let t = threshold.temperature_f as f64;
    // Continuity correction: Kalshi temperatures are integer °F. "≥ 70" is
    // "rounded high ≥ 70", i.e. observed > 69.5. Small effect, but matters
    // near the threshold.
    let p_at_or_above = 1.0 - normal_cdf((t - 0.5 - mu) / sigma);
    let p_at_or_below = normal_cdf((t + 0.5 - mu) / sigma);
    let p = match threshold.direction {
        ThresholdDirection::AtOrAbove => p_at_or_above,
        ThresholdDirection::AtOrBelow => p_at_or_below,
    };
    p.clamp(0.0, 1.0)
}

/// Standard Normal CDF Φ(z). Implemented via the Abramowitz–Stegun 26.2.17
/// approximation (max abs error ≈ 7.5e-8) — accurate enough for sub-cent
/// probability differences.
///
/// ```
/// use weather_pricing::normal_cdf;
/// // Median of the standard Normal — exactly 0.5.
/// assert!((normal_cdf(0.0) - 0.5).abs() < 1e-9);
/// // ±1σ ≈ 0.8413 / 0.1587 — the canonical "1 standard deviation" tail.
/// assert!((normal_cdf(1.0) - 0.8413).abs() < 1e-3);
/// // CDF is symmetric: Φ(z) + Φ(−z) = 1.
/// assert!((normal_cdf(2.5) + normal_cdf(-2.5) - 1.0).abs() < 1e-9);
/// ```
pub fn normal_cdf(z: f64) -> f64 {
    if z.is_nan() {
        return f64::NAN;
    }
    let sign = if z < 0.0 { -1.0 } else { 1.0 };
    let x = (z / std::f64::consts::SQRT_2).abs();
    let p = 0.3275911;
    let a1 = 0.254829592;
    let a2 = -0.284496736;
    let a3 = 1.421413741;
    let a4 = -1.453152027;
    let a5 = 1.061405429;
    let t = 1.0 / (1.0 + p * x);
    let t2 = t * t;
    let t3 = t2 * t;
    let t4 = t3 * t;
    let t5 = t4 * t;
    let erf_x = 1.0 - (a1 * t + a2 * t2 + a3 * t3 + a4 * t4 + a5 * t5) * (-(x * x)).exp();
    0.5 * (1.0 + sign * erf_x)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, NaiveDate, Utc};
    use weather_types::{Forecast, ForecastPeriod, TempStat, ThresholdDirection, WeatherThreshold};

    fn dt(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    fn nyc_july4_forecast(forecast_temp: i32) -> Forecast {
        Forecast {
            lat: 40.78,
            lon: -73.97,
            fetched_at: dt("2026-07-03T12:00:00Z"),
            generated_at: None,
            periods: vec![
                ForecastPeriod {
                    name: "Saturday".into(),
                    is_daytime: true,
                    temperature_f: forecast_temp,
                    precipitation_probability_pct: None,
                    start_time: dt("2026-07-04T10:00:00Z"),
                    end_time: dt("2026-07-04T22:00:00Z"),
                    detailed_forecast: "".into(),
                },
                ForecastPeriod {
                    name: "Saturday Night".into(),
                    is_daytime: false,
                    temperature_f: forecast_temp - 18,
                    precipitation_probability_pct: None,
                    start_time: dt("2026-07-04T22:00:00Z"),
                    end_time: dt("2026-07-05T10:00:00Z"),
                    detailed_forecast: "".into(),
                },
            ],
        }
    }

    fn high_threshold(t: i32, dir: ThresholdDirection) -> WeatherThreshold {
        WeatherThreshold {
            city: "NY".into(),
            stat: TempStat::DailyHigh,
            direction: dir,
            temperature_f: t,
            date: NaiveDate::from_ymd_opt(2026, 7, 4).unwrap(),
        }
    }

    #[test]
    fn normal_cdf_known_values() {
        assert!((normal_cdf(0.0) - 0.5).abs() < 1e-6);
        assert!((normal_cdf(1.0) - 0.8413).abs() < 1e-3);
        assert!((normal_cdf(-1.0) - 0.1587).abs() < 1e-3);
        assert!((normal_cdf(2.0) + normal_cdf(-2.0) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn at_threshold_is_near_50_50() {
        let f = nyc_july4_forecast(70);
        let m = high_threshold(70, ThresholdDirection::AtOrAbove);
        let p = price_market(&m, &f).unwrap();
        let pf: f64 = p.yes_probability.try_into().unwrap();
        assert!(pf > 0.5 && pf < 0.7, "got {}", pf);
    }

    #[test]
    fn far_above_threshold_is_near_1() {
        let f = nyc_july4_forecast(90);
        let m = high_threshold(70, ThresholdDirection::AtOrAbove);
        let p = price_market(&m, &f).unwrap();
        let pf: f64 = p.yes_probability.try_into().unwrap();
        assert!(pf > 0.99, "got {}", pf);
        assert_eq!(p.forecast_temp_f, 90);
        assert_eq!(p.settlement_station, "KNYC");
    }

    #[test]
    fn far_below_threshold_is_near_0() {
        let f = nyc_july4_forecast(50);
        let m = high_threshold(70, ThresholdDirection::AtOrAbove);
        let p = price_market(&m, &f).unwrap();
        let pf: f64 = p.yes_probability.try_into().unwrap();
        assert!(pf < 0.01, "got {}", pf);
    }

    #[test]
    fn at_or_below_inverts_correctly() {
        let f = nyc_july4_forecast(90);
        let m = high_threshold(70, ThresholdDirection::AtOrBelow);
        let p = price_market(&m, &f).unwrap();
        let pf: f64 = p.yes_probability.try_into().unwrap();
        assert!(pf < 0.01, "got {}", pf);
    }

    #[test]
    fn unknown_city_returns_error() {
        let f = nyc_july4_forecast(90);
        let mut m = high_threshold(70, ThresholdDirection::AtOrAbove);
        m.city = "UNKNOWN".into();
        assert!(matches!(
            price_market(&m, &f),
            Err(PricingError::UnknownCity(_))
        ));
    }

    #[test]
    fn missing_period_returns_error() {
        let mut f = nyc_july4_forecast(72);
        f.periods.retain(|p| !p.is_daytime);
        let m = high_threshold(70, ThresholdDirection::AtOrAbove);
        assert!(matches!(
            price_market(&m, &f),
            Err(PricingError::NoMatchingForecastPeriod)
        ));
    }

    #[test]
    fn horizon_widens_sigma() {
        let near = sigma_for_horizon(1);
        let far = sigma_for_horizon(7);
        assert!(near < far);
        let p_near = 1.0 - normal_cdf((70.0 - 75.0) / near);
        let p_far = 1.0 - normal_cdf((70.0 - 75.0) / far);
        assert!(p_near > p_far);
    }

    #[test]
    fn sigma_override_is_used_in_place_of_horizon_table() {
        let f = nyc_july4_forecast(75);
        let m = high_threshold(75, ThresholdDirection::AtOrAbove);
        let with_override =
            price_market_with_sigma(&m, &f, Some((4.0, SIGMA_SOURCE_GEFS_ENSEMBLE))).unwrap();
        let without_override = price_market_with_sigma(&m, &f, None).unwrap();

        // Override = 4.0 should be reflected verbatim, regardless of horizon.
        assert!((with_override.sigma_f - 4.0).abs() < 1e-9);
        assert_eq!(with_override.sigma_source, "gefs_ensemble");
        // Static fallback uses sigma_for_horizon(horizon). For July 4 with
        // a forecast fetched July 3 (horizon = 1), that's 2.0.
        assert!((without_override.sigma_f - 2.0).abs() < 1e-9);
        assert_eq!(without_override.sigma_source, "static");

        // A wider σ pushes P(YES) at-the-money toward 0.5.
        let pf_with: f64 = with_override.yes_probability.try_into().unwrap();
        let pf_without: f64 = without_override.yes_probability.try_into().unwrap();
        // Wider σ → at the strike, the same continuity correction yields a
        // probability closer to 0.5 than the narrower σ does.
        assert!(
            (pf_with - 0.5).abs() < (pf_without - 0.5).abs(),
            "with σ=4.0 P should be closer to 0.5 than with σ=2.0; got {} vs {}",
            pf_with,
            pf_without
        );
    }

    fn low_threshold(t: i32, dir: ThresholdDirection) -> WeatherThreshold {
        WeatherThreshold {
            city: "NY".into(),
            stat: TempStat::DailyLow,
            direction: dir,
            temperature_f: t,
            date: NaiveDate::from_ymd_opt(2026, 7, 4).unwrap(),
        }
    }

    #[test]
    fn lock_fires_when_running_high_crosses_at_or_above_strike() {
        let m = high_threshold(75, ThresholdDirection::AtOrAbove);
        // running high = 75 → ≥ 75 - 0.5 → locked.
        let p = price_market_with_lock(&m, 75.0).unwrap().expect("locked");
        let pf: f64 = p.yes_probability.try_into().unwrap();
        assert!((pf - METAR_LOCK_YES_PROBABILITY).abs() < 1e-9);
        assert_eq!(p.sigma_source, "metar_lock");
        assert_eq!(p.sigma_f, 0.0);
        assert_eq!(p.settlement_station, "KNYC");
    }

    #[test]
    fn lock_does_not_fire_when_running_high_below_strike() {
        let m = high_threshold(75, ThresholdDirection::AtOrAbove);
        // running high = 74 → < 75 - 0.5 = 74.5 → not locked.
        assert!(price_market_with_lock(&m, 74.0).unwrap().is_none());
    }

    #[test]
    fn lock_fires_for_daily_low_at_or_below() {
        let m = low_threshold(60, ThresholdDirection::AtOrBelow);
        // running low = 60 → ≤ 60 + 0.5 → locked.
        let p = price_market_with_lock(&m, 60.0).unwrap().expect("locked");
        assert_eq!(p.sigma_source, "metar_lock");
    }

    #[test]
    fn lock_does_not_fire_for_opposite_directions_in_v1() {
        // DailyHigh + AtOrBelow → "high ≤ T". A running high crossing T+
        // is a NO-side lock that we explicitly defer; YES-only in v1.
        let m = high_threshold(75, ThresholdDirection::AtOrBelow);
        assert!(price_market_with_lock(&m, 90.0).unwrap().is_none());
        // Symmetrically, DailyLow + AtOrAbove → "low ≥ T". A running low
        // dipping below T- locks NO; deferred.
        let m = low_threshold(60, ThresholdDirection::AtOrAbove);
        assert!(price_market_with_lock(&m, 50.0).unwrap().is_none());
    }

    #[test]
    fn sigma_source_tag_round_trips_from_caller() {
        let f = nyc_july4_forecast(75);
        let m = high_threshold(75, ThresholdDirection::AtOrAbove);
        let blended =
            price_market_with_sigma(&m, &f, Some((3.0, SIGMA_SOURCE_GEFS_ECMWF_BLEND))).unwrap();
        let ecmwf =
            price_market_with_sigma(&m, &f, Some((2.5, SIGMA_SOURCE_ECMWF_ENSEMBLE))).unwrap();
        assert_eq!(blended.sigma_source, "gefs_ecmwf_blend");
        assert_eq!(ecmwf.sigma_source, "ecmwf_ensemble");
    }

    #[test]
    fn price_market_default_path_matches_explicit_none_override() {
        let f = nyc_july4_forecast(82);
        let m = high_threshold(75, ThresholdDirection::AtOrAbove);
        let a = price_market(&m, &f).unwrap();
        let b = price_market_with_sigma(&m, &f, None).unwrap();
        assert_eq!(a.yes_probability, b.yes_probability);
        assert_eq!(a.sigma_f, b.sigma_f);
        assert_eq!(a.forecast_temp_f, b.forecast_temp_f);
        assert_eq!(a.horizon_days, b.horizon_days);
    }
}
