//! Iowa State IEM CLI (Climate Report) fetcher.
//!
//! NWS CLI products are the daily climate summary issued the morning after a
//! station's "climate day". They are also what Kalshi uses to settle weather
//! markets — Kalshi's help-center notes that final settlement values come
//! from the issuing WFO's CLI text product. So the CLI is the *ground truth*
//! we need to (a) reconcile against past Kalshi settlements and (b) feed
//! into backtests as the realised outcome.
//!
//! Iowa State's Mesonet hosts a parsed JSON view of every CLI product:
//!   `https://mesonet.agron.iastate.edu/json/cli.py?station=<ICAO>&fmt=json&year=<Y>&month=<M>`
//! Verified live against `KNYC` 2026-01: returns `{"results": [...]}` with
//! one entry per day, integer `high` / `low` in °F, plus precip and snow.
//!
//! v1 only extracts (date, station, high_f, low_f) — that's all the
//! reconciliation against KXHIGH/KXLOW markets needs. We can extend later
//! when precip markets enter scope (Phase 4).
//!
//! Important: `?year=`/`?month=` are required for current-year data. Without
//! them the endpoint defaults to *2019* (the earliest year IEM has parsed),
//! not the current year. `fetch_recent` handles this by walking the months
//! spanned by the requested window.

use chrono::{Datelike, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum CliError {
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("IEM responded {status} for {url}: {body}")]
    Status {
        status: u16,
        url: String,
        body: String,
    },
    #[error("JSON parse error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("CLI payload missing field: {0}")]
    Missing(&'static str),
    #[error("CLI payload has unparseable date '{0}'")]
    BadDate(String),
}

/// One day's CLI summary for a station. `high_f` / `low_f` may be `None` when
/// IEM reports the value as `"M"` (missing) — rare, but happens when the
/// observing station has a data outage.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CliReport {
    /// Station ICAO, e.g. `"KNYC"`.
    pub station: String,
    /// Climate-day the report covers (local-standard-time day, per NWS
    /// convention — same standard-time day Kalshi settles on).
    pub date: NaiveDate,
    /// Daily high °F. Integer because CLI products report integers.
    pub high_f: Option<i32>,
    /// Daily low °F.
    pub low_f: Option<i32>,
}

/// Parse the IEM `/json/cli.py` payload into a flat list of `CliReport`.
/// Tolerant of mixed types: IEM occasionally encodes missing values as the
/// string `"M"` rather than null.
pub fn parse_cli_json(payload: &str) -> Result<Vec<CliReport>, CliError> {
    let v: serde_json::Value = serde_json::from_str(payload)?;
    let arr = v
        .get("results")
        .and_then(|r| r.as_array())
        .ok_or(CliError::Missing("results"))?;

    let mut out = Vec::with_capacity(arr.len());
    for entry in arr {
        let station = entry
            .get("station")
            .and_then(|v| v.as_str())
            .ok_or(CliError::Missing("station"))?
            .to_string();
        let valid_str = entry
            .get("valid")
            .and_then(|v| v.as_str())
            .ok_or(CliError::Missing("valid"))?;
        let date = NaiveDate::parse_from_str(valid_str, "%Y-%m-%d")
            .map_err(|_| CliError::BadDate(valid_str.to_string()))?;
        let high_f = extract_optional_int(entry.get("high"));
        let low_f = extract_optional_int(entry.get("low"));
        out.push(CliReport {
            station,
            date,
            high_f,
            low_f,
        });
    }
    Ok(out)
}

/// IEM mixes integers and the string `"M"` (and rarely `null`) for missing
/// values. Treat all three as `None`; otherwise coerce to `i32`.
fn extract_optional_int(v: Option<&serde_json::Value>) -> Option<i32> {
    match v {
        Some(serde_json::Value::Number(n)) => n.as_i64().map(|x| x as i32),
        _ => None,
    }
}

pub struct IemCliClient {
    http: reqwest::Client,
    base_url: String,
}

impl IemCliClient {
    /// `base_url` is typically `"https://mesonet.agron.iastate.edu"`.
    pub fn new(base_url: String) -> Self {
        Self {
            http: reqwest::Client::new(),
            base_url,
        }
    }

    pub fn iem_default() -> Self {
        Self::new("https://mesonet.agron.iastate.edu".to_string())
    }

    /// Fetch one calendar month of CLI rows for a station. IEM's response is
    /// already trimmed to the (year, month) you ask for.
    pub async fn fetch_month(
        &self,
        station_icao: &str,
        year: i32,
        month: u32,
    ) -> Result<Vec<CliReport>, CliError> {
        let url = format!(
            "{}/json/cli.py?station={}&fmt=json&year={}&month={}",
            self.base_url, station_icao, year, month
        );
        let resp = self.http.get(&url).send().await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(CliError::Status {
                status: status.as_u16(),
                url,
                body,
            });
        }
        let bytes = resp.bytes().await?;
        let text = std::str::from_utf8(&bytes).map_err(|e| {
            serde_json::Error::io(std::io::Error::new(std::io::ErrorKind::InvalidData, e))
        })?;
        parse_cli_json(text)
    }

    /// Fetch the last `days` of CLI for a station, walking each calendar
    /// month the window touches and filtering to the date range. UTC "today"
    /// is the upper bound; CLI for "today" is generally not yet issued, so
    /// expect the result to end yesterday.
    pub async fn fetch_recent(
        &self,
        station_icao: &str,
        days: u32,
    ) -> Result<Vec<CliReport>, CliError> {
        let today = Utc::now().date_naive();
        let start = today - chrono::Duration::days(days as i64);
        let mut out = Vec::new();
        for (year, month) in months_spanning(start, today) {
            let mut chunk = self.fetch_month(station_icao, year, month).await?;
            chunk.retain(|r| r.date >= start && r.date <= today);
            out.extend(chunk);
        }
        out.sort_by_key(|r| r.date);
        Ok(out)
    }
}

/// All (year, month) tuples whose calendar month touches `[start, end]`.
/// Inclusive on both ends; assumes `start <= end`.
fn months_spanning(start: NaiveDate, end: NaiveDate) -> Vec<(i32, u32)> {
    let mut out = Vec::new();
    let (mut y, mut m) = (start.year(), start.month());
    let end_y = end.year();
    let end_m = end.month();
    loop {
        out.push((y, m));
        if (y, m) == (end_y, end_m) {
            break;
        }
        m += 1;
        if m > 12 {
            m = 1;
            y += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Trimmed real IEM response (KNYC 2019-01-01..2019-01-03, captured
    /// 2026-05-03). The first entry has full data; the second is fabricated
    /// with `"M"` in the `high` slot to exercise the missing-value path
    /// (IEM does emit `"M"` for some fields and we want to be defensive).
    const FIXTURE_KNYC: &str = r#"{
        "results": [
            {
                "station": "KNYC",
                "valid": "2019-01-01",
                "state": "NY",
                "name": "NEW YORK CITY",
                "high": 58,
                "low": 39,
                "high_time": "1122 AM",
                "low_time": "1159 PM",
                "precip": 0.06
            },
            {
                "station": "KNYC",
                "valid": "2019-01-02",
                "state": "NY",
                "name": "NEW YORK CITY",
                "high": "M",
                "low": 35
            },
            {
                "station": "KNYC",
                "valid": "2019-01-03",
                "state": "NY",
                "name": "NEW YORK CITY",
                "high": 33,
                "low": null
            }
        ]
    }"#;

    #[test]
    fn parses_real_iem_response_shape() {
        let rows = parse_cli_json(FIXTURE_KNYC).unwrap();
        assert_eq!(rows.len(), 3);

        assert_eq!(rows[0].station, "KNYC");
        assert_eq!(rows[0].date, NaiveDate::from_ymd_opt(2019, 1, 1).unwrap());
        assert_eq!(rows[0].high_f, Some(58));
        assert_eq!(rows[0].low_f, Some(39));
    }

    #[test]
    fn missing_high_as_string_m_round_trips_to_none() {
        let rows = parse_cli_json(FIXTURE_KNYC).unwrap();
        assert_eq!(rows[1].high_f, None);
        assert_eq!(rows[1].low_f, Some(35));
    }

    #[test]
    fn missing_low_as_null_round_trips_to_none() {
        let rows = parse_cli_json(FIXTURE_KNYC).unwrap();
        assert_eq!(rows[2].high_f, Some(33));
        assert_eq!(rows[2].low_f, None);
    }

    #[test]
    fn malformed_payload_is_an_error() {
        assert!(parse_cli_json("not json").is_err());
        // `results` must be an array.
        assert!(parse_cli_json(r#"{"results": "nope"}"#).is_err());
        // `valid` must parse as YYYY-MM-DD.
        let bad = r#"{"results": [{"station": "X", "valid": "yesterday"}]}"#;
        assert!(matches!(parse_cli_json(bad), Err(CliError::BadDate(_))));
    }

    #[test]
    fn months_spanning_within_one_month() {
        let s = NaiveDate::from_ymd_opt(2026, 4, 5).unwrap();
        let e = NaiveDate::from_ymd_opt(2026, 4, 28).unwrap();
        assert_eq!(months_spanning(s, e), vec![(2026, 4)]);
    }

    #[test]
    fn months_spanning_crosses_month_boundary() {
        let s = NaiveDate::from_ymd_opt(2026, 4, 28).unwrap();
        let e = NaiveDate::from_ymd_opt(2026, 5, 5).unwrap();
        assert_eq!(months_spanning(s, e), vec![(2026, 4), (2026, 5)]);
    }

    #[test]
    fn months_spanning_crosses_year_boundary() {
        let s = NaiveDate::from_ymd_opt(2025, 12, 28).unwrap();
        let e = NaiveDate::from_ymd_opt(2026, 2, 3).unwrap();
        assert_eq!(
            months_spanning(s, e),
            vec![(2025, 12), (2026, 1), (2026, 2)]
        );
    }

    /// Live integration test against the real IEM endpoint. Skipped by
    /// default to keep `cargo test` offline-clean. Run with:
    ///   `cargo test -p weather-forecast -- --ignored cli::live`
    #[tokio::test]
    #[ignore]
    async fn cli_live_fetch_knyc_2026_january() {
        let client = IemCliClient::iem_default();
        let rows = client.fetch_month("KNYC", 2026, 1).await.unwrap();
        assert!(!rows.is_empty(), "January 2026 should have CLI rows");
        for r in &rows {
            assert_eq!(r.station, "KNYC");
            assert_eq!(r.date.year(), 2026);
            assert_eq!(r.date.month(), 1);
            assert!(r.high_f.is_some() || r.low_f.is_some());
        }
    }
}
