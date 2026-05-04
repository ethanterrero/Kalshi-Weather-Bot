//! Open-Meteo historical-forecast fetcher.
//!
//! Substrate for Phase A model calibration: pull what GFS *predicted* for
//! a date in the past, then compare to the realised CLI on the same day.
//! Open-Meteo's `historical-forecast-api.open-meteo.com/v1/forecast`
//! archives the deterministic GFS run made for each past date — typically
//! the same-day 00z run. So this delivers horizon = 0 forecasts; multi-
//! horizon archive needs a separate run-time-indexed source (out of
//! scope for v1 calibration).
//!
//! Verified live shape (KNYC, 2025-04-01..2025-04-03):
//!   GET /v1/forecast?latitude=40.78&longitude=-73.97
//!       &start_date=2025-04-01&end_date=2025-04-03
//!       &hourly=temperature_2m&temperature_unit=fahrenheit
//!       &models=gfs_seamless&timezone=GMT
//!   →   { "hourly": {
//!           "time": ["2025-04-01T00:00", ...],
//!           "temperature_2m": [44.6, ...]
//!         } }
//!
//! Times are naive UTC strings (`timezone=GMT` makes that explicit).

use chrono::{DateTime, NaiveDate, NaiveDateTime, Utc};
use thiserror::Error;
use weather_types::{daily_high_window_utc, daily_low_window_utc, CitySpec};

#[derive(Debug, Error)]
pub enum HistoricalForecastError {
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
    #[error("payload missing field: {0}")]
    Missing(&'static str),
    #[error("payload has unparseable timestamp '{0}'")]
    BadTime(String),
}

/// Hourly archived deterministic forecast for a (lat, lon) over a date
/// range. `times` and `temperature_2m` are aligned by index.
#[derive(Debug, Clone)]
pub struct HistoricalForecast {
    pub fetched_at: DateTime<Utc>,
    pub times: Vec<DateTime<Utc>>,
    /// Hourly 2m temperatures (°F). `None` for slots Open-Meteo couldn't
    /// reconstruct (rare; happens at the very edge of the archive).
    pub temperature_2m: Vec<Option<f64>>,
}

impl HistoricalForecast {
    /// Daily high (°F, integer) inside the Kalshi standard-time window
    /// for `(city, date)`. Uses the same `daily_high_window_utc` helper
    /// `weather-pricing` uses, so the bucket lines up exactly with what
    /// the bot would price against.
    pub fn daily_high(&self, city: &CitySpec, date: NaiveDate) -> Option<i32> {
        let (start, end) = daily_high_window_utc(city, date);
        self.daily_extreme(start, end, true)
    }

    pub fn daily_low(&self, city: &CitySpec, date: NaiveDate) -> Option<i32> {
        let (start, end) = daily_low_window_utc(city, date);
        self.daily_extreme(start, end, false)
    }

    fn daily_extreme(
        &self,
        window_start: DateTime<Utc>,
        window_end: DateTime<Utc>,
        take_max: bool,
    ) -> Option<i32> {
        let mut extreme: Option<f64> = None;
        for (i, t) in self.times.iter().enumerate() {
            if *t < window_start || *t >= window_end {
                continue;
            }
            let Some(temp) = self.temperature_2m.get(i).copied().flatten() else {
                continue;
            };
            extreme = Some(match (extreme, take_max) {
                (None, _) => temp,
                (Some(prev), true) => prev.max(temp),
                (Some(prev), false) => prev.min(temp),
            });
        }
        // Round to nearest integer; CLI extremes are integer °F so the
        // comparison space is integer.
        extreme.map(|t| t.round() as i32)
    }
}

/// Parse Open-Meteo's `/v1/forecast` JSON (under historical-forecast-api).
pub fn parse_historical_json(payload: &str) -> Result<HistoricalForecast, HistoricalForecastError> {
    let v: serde_json::Value = serde_json::from_str(payload)?;
    let hourly = v
        .get("hourly")
        .ok_or(HistoricalForecastError::Missing("hourly"))?;

    let times_v = hourly
        .get("time")
        .and_then(|x| x.as_array())
        .ok_or(HistoricalForecastError::Missing("hourly.time"))?;
    let mut times = Vec::with_capacity(times_v.len());
    for t in times_v {
        let s = t
            .as_str()
            .ok_or(HistoricalForecastError::Missing("hourly.time[i] string"))?;
        let naive = NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M")
            .map_err(|_| HistoricalForecastError::BadTime(s.to_string()))?;
        times.push(DateTime::<Utc>::from_naive_utc_and_offset(naive, Utc));
    }

    let temps_v = hourly
        .get("temperature_2m")
        .and_then(|x| x.as_array())
        .ok_or(HistoricalForecastError::Missing("hourly.temperature_2m"))?;
    let mut temperature_2m = Vec::with_capacity(temps_v.len());
    for el in temps_v {
        match el {
            serde_json::Value::Number(n) => temperature_2m.push(n.as_f64()),
            _ => temperature_2m.push(None),
        }
    }

    Ok(HistoricalForecast {
        fetched_at: Utc::now(),
        times,
        temperature_2m,
    })
}

pub struct HistoricalForecastClient {
    http: reqwest::Client,
    base_url: String,
}

impl HistoricalForecastClient {
    pub fn new(base_url: String) -> Self {
        Self {
            http: reqwest::Client::new(),
            base_url,
        }
    }

    pub fn open_meteo() -> Self {
        Self::new("https://historical-forecast-api.open-meteo.com".to_string())
    }

    /// Fetch GFS deterministic forecast hourly temperatures over `[start, end]`
    /// for a (lat, lon). Open-Meteo caps single-call windows at ~6 months
    /// in practice; for longer stretches, batch the calls externally.
    pub async fn fetch_range(
        &self,
        lat: f64,
        lon: f64,
        start: NaiveDate,
        end: NaiveDate,
    ) -> Result<HistoricalForecast, HistoricalForecastError> {
        let url = format!(
            "{}/v1/forecast?latitude={}&longitude={}&start_date={}&end_date={}&hourly=temperature_2m&temperature_unit=fahrenheit&models=gfs_seamless&timezone=GMT",
            self.base_url, lat, lon, start, end
        );
        let resp = self.http.get(&url).send().await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(HistoricalForecastError::Status {
                status: status.as_u16(),
                url,
                body,
            });
        }
        let bytes = resp.bytes().await?;
        let text = std::str::from_utf8(&bytes).map_err(|e| {
            serde_json::Error::io(std::io::Error::new(std::io::ErrorKind::InvalidData, e))
        })?;
        parse_historical_json(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use weather_types::lookup_city;

    /// Synthetic 24-hour fixture covering a NYC standard-time climate day.
    /// July 4 in NY: window starts 2026-07-04 05:00Z (00:00 EST) and runs
    /// 24h. Temps peak at 90°F mid-afternoon; min 70°F early morning.
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

    #[test]
    fn parses_real_open_meteo_archive_shape() {
        let f = parse_historical_json(FIXTURE_24H).unwrap();
        assert_eq!(f.times.len(), 24);
        assert_eq!(f.temperature_2m.len(), 24);
        assert!((f.temperature_2m[12].unwrap() - 90.0).abs() < 1e-9);
    }

    #[test]
    fn daily_high_picks_max_inside_standard_time_window() {
        let f = parse_historical_json(FIXTURE_24H).unwrap();
        let nyc = lookup_city("NY").expect("NY in cities table");
        let date = NaiveDate::from_ymd_opt(2026, 7, 4).unwrap();
        // Peak in the fixture is 90.0; rounds to 90.
        assert_eq!(f.daily_high(nyc, date), Some(90));
    }

    #[test]
    fn daily_low_picks_min_inside_low_window() {
        let f = parse_historical_json(FIXTURE_24H).unwrap();
        let nyc = lookup_city("NY").expect("NY in cities table");
        let date = NaiveDate::from_ymd_opt(2026, 7, 4).unwrap();
        // Minimum in the fixture is 68.6 (07:00Z = 03:00 EST), inside
        // the night window. Rounds to 69.
        let low = f.daily_low(nyc, date).expect("inside horizon");
        assert!((68..=70).contains(&low), "expected low ~69, got {}", low);
    }

    #[test]
    fn extreme_outside_horizon_returns_none() {
        let f = parse_historical_json(FIXTURE_24H).unwrap();
        let nyc = lookup_city("NY").unwrap();
        let date = NaiveDate::from_ymd_opt(2030, 1, 1).unwrap();
        assert!(f.daily_high(nyc, date).is_none());
        assert!(f.daily_low(nyc, date).is_none());
    }

    #[test]
    fn null_temperature_is_skipped_not_zero() {
        // Insert a null in the middle to ensure we don't accidentally
        // treat it as a 0°F observation (which would always be the min).
        let payload = r#"{
            "hourly": {
                "time": ["2026-07-04T16:00", "2026-07-04T17:00"],
                "temperature_2m": [88.0, null]
            }
        }"#;
        let f = parse_historical_json(payload).unwrap();
        assert_eq!(f.temperature_2m, vec![Some(88.0), None]);
    }

    #[test]
    fn malformed_payload_is_an_error() {
        assert!(parse_historical_json("not json").is_err());
        assert!(parse_historical_json(r#"{"hourly": null}"#).is_err());
        let bad = r#"{"hourly": {"time": ["yesterday"], "temperature_2m": [70]}}"#;
        assert!(matches!(
            parse_historical_json(bad),
            Err(HistoricalForecastError::BadTime(_))
        ));
    }

    /// Live integration test against Open-Meteo's historical-forecast API.
    /// Skipped by default. Run with:
    ///   `cargo test -p weather-forecast -- --ignored historical_forecast::live`
    #[tokio::test]
    #[ignore]
    async fn historical_forecast_live_fetch_knyc_april_2025() {
        let client = HistoricalForecastClient::open_meteo();
        let f = client
            .fetch_range(
                40.78,
                -73.97,
                NaiveDate::from_ymd_opt(2025, 4, 1).unwrap(),
                NaiveDate::from_ymd_opt(2025, 4, 3).unwrap(),
            )
            .await
            .unwrap();
        // 3 days × 24 hours = 72 entries, give or take a boundary hour.
        assert!(
            (70..=73).contains(&f.times.len()),
            "expected ~72 entries, got {}",
            f.times.len()
        );
        let nyc = lookup_city("NY").unwrap();
        let date = NaiveDate::from_ymd_opt(2025, 4, 2).unwrap();
        // April 2 in NYC has *some* sane high — sanity-check the range.
        let high = f.daily_high(nyc, date).expect("inside fetched range");
        assert!(
            (20..=110).contains(&high),
            "implausible April 2 NYC high: {}",
            high
        );
    }
}
