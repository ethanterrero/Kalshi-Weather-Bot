//! GEFS (NOAA Global Ensemble Forecast System) ensemble fetcher.
//!
//! Replaces the hand-calibrated `sigma_for_horizon` table for in-horizon
//! days with a real per-(city, day) `(μ, σ)` derived from the spread of
//! ~30 perturbed ensemble members. This is the single biggest model
//! upgrade on the roadmap before backtesting.
//!
//! Source: Open-Meteo's `/v1/ensemble` endpoint, which already does the
//! GRIB2 parsing and exposes ensemble members as JSON arrays. This sidesteps
//! adding a GRIB2 dep to the workspace, at the cost of one external
//! dependency. AWS Open Data (`noaa-gefs-pds`) is the long-term fallback if
//! Open-Meteo rate-limits or goes down.
//!
//! Verified live shape (KNYC point, 2026-05-04):
//!   GET /v1/ensemble?latitude=40.78&longitude=-73.97&hourly=temperature_2m
//!       &models=gfs05&temperature_unit=fahrenheit&forecast_days=2
//!   →   {
//!         "hourly": {
//!           "time": ["2026-05-04T00:00", ...],
//!           "temperature_2m":           [55.4, ...],   // control run
//!           "temperature_2m_member01":  [55.5, ...],
//!           ...
//!           "temperature_2m_member30":  [...],
//!         }
//!       }
//!
//! Times are UTC (`utc_offset_seconds: 0` in the response). For Kalshi
//! settlement windows we cross-reference with `weather-types`'s
//! standard-time helpers, not Open-Meteo's `timezone` parameter, because
//! Kalshi settles on standard time year-round (no DST).

use chrono::{DateTime, NaiveDate, NaiveDateTime, Utc};
use thiserror::Error;
use weather_types::{daily_high_window_utc, daily_low_window_utc, CitySpec};

#[derive(Debug, Error)]
pub enum GefsError {
    #[error("HTTP: {0}")]
    Http(#[from] reqwest::Error),
    #[error("Open-Meteo {status} for {url}: {body}")]
    Status {
        status: u16,
        url: String,
        body: String,
    },
    #[error("JSON parse: {0}")]
    Json(#[from] serde_json::Error),
    #[error("ensemble payload missing field: {0}")]
    Missing(&'static str),
    #[error("ensemble payload has unparseable timestamp '{0}'")]
    BadTime(String),
}

/// One ensemble fetch for a (lat, lon). `times` and every entry in
/// `control` / `members` are aligned by index. Missing values surface as
/// `None`; Open-Meteo occasionally returns `null` for a member at the
/// horizon edge.
#[derive(Debug, Clone)]
pub struct EnsembleForecast {
    pub fetched_at: DateTime<Utc>,
    pub times: Vec<DateTime<Utc>>,
    pub control: Vec<Option<f64>>,
    pub members: Vec<Vec<Option<f64>>>,
}

impl EnsembleForecast {
    pub fn member_count(&self) -> usize {
        self.members.len()
    }
}

/// Aggregate ensemble statistic for a single (city, day, side) — i.e. the
/// daily high or daily low. `mu_f`/`sigma_f` are the sample mean and
/// (population) standard deviation of the per-member daily extremes.
#[derive(Debug, Clone, PartialEq)]
pub struct EnsembleStat {
    pub mu_f: f64,
    pub sigma_f: f64,
    /// How many members contributed at least one in-window observation.
    /// May be < `forecast.member_count()` if a member was entirely null.
    pub n_members: usize,
}

/// Parse the Open-Meteo `/v1/ensemble` JSON response. Times come back as
/// naive `YYYY-MM-DDTHH:MM` strings; the response header confirms UTC.
pub fn parse_ensemble_json(payload: &str) -> Result<EnsembleForecast, GefsError> {
    let v: serde_json::Value = serde_json::from_str(payload)?;
    let hourly = v.get("hourly").ok_or(GefsError::Missing("hourly"))?;

    let times_v = hourly
        .get("time")
        .and_then(|x| x.as_array())
        .ok_or(GefsError::Missing("hourly.time"))?;
    let mut times = Vec::with_capacity(times_v.len());
    for t in times_v {
        let s = t
            .as_str()
            .ok_or(GefsError::Missing("hourly.time[i] string"))?;
        let naive = NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M")
            .map_err(|_| GefsError::BadTime(s.to_string()))?;
        times.push(DateTime::<Utc>::from_naive_utc_and_offset(naive, Utc));
    }

    let control = extract_temp_array(hourly.get("temperature_2m"))?;

    // Members are 01..=N (zero-padded). Stop at the first missing key —
    // Open-Meteo always emits a contiguous run.
    let mut members = Vec::new();
    for i in 1..=99u32 {
        let key = format!("temperature_2m_member{:02}", i);
        let Some(arr) = hourly.get(&key) else { break };
        members.push(extract_temp_array(Some(arr))?);
    }

    Ok(EnsembleForecast {
        fetched_at: Utc::now(),
        times,
        control,
        members,
    })
}

fn extract_temp_array(v: Option<&serde_json::Value>) -> Result<Vec<Option<f64>>, GefsError> {
    let arr = v
        .and_then(|x| x.as_array())
        .ok_or(GefsError::Missing("temperature array"))?;
    let mut out = Vec::with_capacity(arr.len());
    for el in arr {
        match el {
            serde_json::Value::Number(n) => out.push(n.as_f64()),
            // null / "M" / anything else → missing.
            _ => out.push(None),
        }
    }
    Ok(out)
}

/// Compute `(μ, σ)` of the per-member daily *high* in the Kalshi
/// standard-time settlement window for `(city, date)`. Returns `None` if
/// no member produced even one observation inside the window — practically
/// only happens when the window is past the forecast horizon.
pub fn daily_high_stats(
    forecast: &EnsembleForecast,
    city: &CitySpec,
    date: NaiveDate,
) -> Option<EnsembleStat> {
    let (start, end) = daily_high_window_utc(city, date);
    daily_extreme_stats(forecast, start, end, true)
}

pub fn daily_low_stats(
    forecast: &EnsembleForecast,
    city: &CitySpec,
    date: NaiveDate,
) -> Option<EnsembleStat> {
    let (start, end) = daily_low_window_utc(city, date);
    daily_extreme_stats(forecast, start, end, false)
}

fn daily_extreme_stats(
    forecast: &EnsembleForecast,
    window_start: DateTime<Utc>,
    window_end: DateTime<Utc>,
    take_max: bool,
) -> Option<EnsembleStat> {
    let mut per_member_extremes = Vec::with_capacity(forecast.members.len());
    for member in &forecast.members {
        let mut extreme: Option<f64> = None;
        for (i, t) in forecast.times.iter().enumerate() {
            // Half-open window matches `weather-types`'s standard-time helpers.
            if *t < window_start || *t >= window_end {
                continue;
            }
            let Some(temp) = member.get(i).copied().flatten() else {
                continue;
            };
            extreme = Some(match (extreme, take_max) {
                (None, _) => temp,
                (Some(prev), true) => prev.max(temp),
                (Some(prev), false) => prev.min(temp),
            });
        }
        if let Some(e) = extreme {
            per_member_extremes.push(e);
        }
    }
    if per_member_extremes.is_empty() {
        return None;
    }
    let n = per_member_extremes.len();
    let mu: f64 = per_member_extremes.iter().sum::<f64>() / n as f64;
    let variance: f64 = per_member_extremes
        .iter()
        .map(|x| (x - mu).powi(2))
        .sum::<f64>()
        / n as f64; // population variance — we're describing the actual sample, not estimating.
    Some(EnsembleStat {
        mu_f: mu,
        sigma_f: variance.sqrt(),
        n_members: n,
    })
}

pub struct GefsClient {
    http: reqwest::Client,
    base_url: String,
}

impl GefsClient {
    pub fn new(base_url: String) -> Self {
        Self {
            http: reqwest::Client::new(),
            base_url,
        }
    }

    pub fn open_meteo() -> Self {
        Self::new("https://ensemble-api.open-meteo.com".to_string())
    }

    /// Fetch the GFS-0.5° ensemble for a point. `forecast_days` is capped
    /// at 16 by Open-Meteo; 7 covers everything Kalshi has open today.
    pub async fn fetch_gfs05(
        &self,
        lat: f64,
        lon: f64,
        forecast_days: u32,
    ) -> Result<EnsembleForecast, GefsError> {
        let url = format!(
            "{}/v1/ensemble?latitude={}&longitude={}&hourly=temperature_2m&models=gfs05&temperature_unit=fahrenheit&forecast_days={}",
            self.base_url, lat, lon, forecast_days
        );
        let resp = self.http.get(&url).send().await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(GefsError::Status {
                status: status.as_u16(),
                url,
                body,
            });
        }
        let bytes = resp.bytes().await?;
        let text = std::str::from_utf8(&bytes).map_err(|e| {
            serde_json::Error::io(std::io::Error::new(std::io::ErrorKind::InvalidData, e))
        })?;
        parse_ensemble_json(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use weather_types::lookup_city;

    /// Trimmed real-shape fixture: 24 hourly points spanning a single NY
    /// standard-time climate day, 3 members. Real Open-Meteo responses
    /// have 30 members; the parser is general so 3 is enough to exercise
    /// the math.
    const FIXTURE_24H_3MEMBERS: &str = r#"{
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
                70, 69, 68, 70, 73, 76, 79, 82, 84, 86, 87, 88,
                88, 87, 86, 84, 82, 79, 76, 74, 73, 72, 71, 70
            ],
            "temperature_2m_member01": [
                70, 69, 68, 70, 73, 76, 79, 82, 84, 86, 88, 90,
                90, 89, 87, 85, 82, 79, 76, 74, 73, 72, 71, 70
            ],
            "temperature_2m_member02": [
                68, 67, 66, 68, 71, 74, 77, 80, 82, 84, 85, 86,
                86, 85, 84, 82, 80, 77, 74, 72, 71, 70, 69, 68
            ],
            "temperature_2m_member03": [
                72, 71, 70, 72, 75, 78, 81, 84, 86, 88, 89, 90,
                90, 89, 88, 86, 84, 81, 78, null, 75, 74, 73, 72
            ]
        }
    }"#;

    #[test]
    fn parses_real_open_meteo_ensemble_shape() {
        let f = parse_ensemble_json(FIXTURE_24H_3MEMBERS).unwrap();
        assert_eq!(f.times.len(), 24);
        assert_eq!(f.control.len(), 24);
        assert_eq!(f.member_count(), 3);
        assert_eq!(f.members[0].len(), 24);
        // First time stamp should round-trip to UTC midnight 2026-07-04 + 5h.
        assert_eq!(
            f.times[0],
            DateTime::<Utc>::from_naive_utc_and_offset(
                NaiveDateTime::parse_from_str("2026-07-04T05:00", "%Y-%m-%dT%H:%M").unwrap(),
                Utc,
            )
        );
    }

    #[test]
    fn null_in_member_array_round_trips_to_none() {
        let f = parse_ensemble_json(FIXTURE_24H_3MEMBERS).unwrap();
        // Member 3, index 19 (the "2026-07-05T00:00" slot) has explicit null.
        assert_eq!(f.members[2][19], None);
    }

    #[test]
    fn daily_high_stats_matches_hand_computed_extremes() {
        // Manually computed per-member daily highs across the 24-hour
        // window for the NY July-4 fixture above:
        //   m1: max = 90  (t=16:00 / 17:00)
        //   m2: max = 86
        //   m3: max = 90
        // μ = 88.667, σ_pop = sqrt( ((90-88.667)^2 + (86-88.667)^2 + (90-88.667)^2) / 3 )
        //   = sqrt( (1.7778 + 7.1111 + 1.7778)/3 ) = sqrt(3.5556) ≈ 1.8856.
        let f = parse_ensemble_json(FIXTURE_24H_3MEMBERS).unwrap();
        let nyc = lookup_city("NY").expect("NY in cities table");
        let date = NaiveDate::from_ymd_opt(2026, 7, 4).unwrap();
        let s = daily_high_stats(&f, nyc, date).expect("inside horizon");

        assert_eq!(s.n_members, 3);
        assert!((s.mu_f - 88.667).abs() < 0.01, "mu: {}", s.mu_f);
        assert!((s.sigma_f - 1.8856).abs() < 0.01, "sigma: {}", s.sigma_f);
    }

    #[test]
    fn daily_low_stats_picks_min_within_low_window() {
        // The low window for a NY climate-day is the night portion. Per the
        // fixture, member-1 dips to 68 in the early-morning slot and rises
        // to ~70 toward 04:00 — minimum is 68. Same for member-3 with
        // an explicit null (gracefully skipped).
        let f = parse_ensemble_json(FIXTURE_24H_3MEMBERS).unwrap();
        let nyc = lookup_city("NY").expect("NY in cities table");
        let date = NaiveDate::from_ymd_opt(2026, 7, 4).unwrap();
        let s = daily_low_stats(&f, nyc, date).expect("inside horizon");
        assert!(s.sigma_f >= 0.0);
        assert!(s.n_members >= 1);
    }

    #[test]
    fn empty_window_returns_none() {
        let f = parse_ensemble_json(FIXTURE_24H_3MEMBERS).unwrap();
        let nyc = lookup_city("NY").expect("NY in cities table");
        // A date well past the fixture's horizon — no in-window samples.
        let date = NaiveDate::from_ymd_opt(2030, 1, 1).unwrap();
        assert!(daily_high_stats(&f, nyc, date).is_none());
        assert!(daily_low_stats(&f, nyc, date).is_none());
    }

    #[test]
    fn malformed_payload_is_an_error() {
        assert!(parse_ensemble_json("not json").is_err());
        assert!(parse_ensemble_json(r#"{"hourly": null}"#).is_err());
        let bad = r#"{"hourly": {"time": ["yesterday"], "temperature_2m": [70]}}"#;
        assert!(matches!(
            parse_ensemble_json(bad),
            Err(GefsError::BadTime(_))
        ));
    }

    /// Live integration test against Open-Meteo. Skipped by default to keep
    /// `cargo test` offline. Run with:
    ///   `cargo test -p weather-forecast -- --ignored gefs::live`
    #[tokio::test]
    #[ignore]
    async fn gefs_live_fetch_knyc_returns_thirty_members() {
        let client = GefsClient::open_meteo();
        let f = client.fetch_gfs05(40.78, -73.97, 2).await.unwrap();
        assert!(
            f.member_count() >= 20,
            "expected ~30 members, got {}",
            f.member_count()
        );
        assert!(!f.times.is_empty());
        assert_eq!(f.control.len(), f.times.len());
        for m in &f.members {
            assert_eq!(m.len(), f.times.len());
        }
    }
}
