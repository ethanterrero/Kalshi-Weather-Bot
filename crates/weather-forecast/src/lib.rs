//! NOAA NWS forecast client + Iowa State IEM CLI fetcher.
//!
//! NWS doesn't take a single (lat, lon) → forecast request. The flow is:
//!   1. `GET /points/{lat},{lon}` → metadata including `properties.forecast`
//!      (a fully-qualified URL to the gridded forecast for that location).
//!   2. `GET <that URL>` → 7-day forecast in alternating day/night periods.
//!
//! NWS rejects requests without a meaningful User-Agent, so all requests go
//! through `NwsClient::new(base_url, user_agent)`.
//!
//! Realised settlement values come from the daily NWS CLI product. See the
//! [`cli`] module for the IEM-backed fetcher and parser.
//!
//! Ensemble (μ, σ) per (city, day) — the substrate for replacing
//! `weather-pricing::sigma_for_horizon` — comes from GEFS via Open-Meteo's
//! `/v1/ensemble` endpoint. See the [`gefs`] module.

pub mod cli;
pub mod gefs;
pub mod historical_forecast;

pub use cli::{parse_cli_json, CliError, CliReport, IemCliClient};
pub use gefs::{
    daily_high_stats, daily_low_stats, parse_ensemble_json, EnsembleForecast, EnsembleStat,
    GefsClient, GefsError,
};
pub use historical_forecast::{
    parse_historical_json, HistoricalForecast, HistoricalForecastClient, HistoricalForecastError,
};

use chrono::{DateTime, Utc};
use serde::Deserialize;
use thiserror::Error;
use tracing::warn;
use weather_types::{Forecast, ForecastPeriod};

#[derive(Debug, Error)]
pub enum ForecastError {
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("NWS responded {status} for {url}: {body}")]
    Status {
        status: u16,
        url: String,
        body: String,
    },
    #[error("NWS response missing field: {0}")]
    Missing(&'static str),
    #[error("JSON parse error: {0}")]
    Json(#[from] serde_json::Error),
}

pub struct NwsClient {
    client: reqwest::Client,
    base_url: String,
    user_agent: String,
}

impl NwsClient {
    pub fn new(base_url: String, user_agent: String) -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url,
            user_agent,
        }
    }

    /// Fetch the 7-day point forecast for a given lat/lon. Two requests:
    /// `/points/{lat},{lon}` to discover the forecast URL, then the forecast
    /// itself.
    pub async fn fetch_point_forecast(
        &self,
        lat: f64,
        lon: f64,
    ) -> Result<Forecast, ForecastError> {
        let points_url = format!("{}/points/{},{}", self.base_url, lat, lon);
        let points: PointsResponse = self.get_json(&points_url).await?;
        let forecast_url = points
            .properties
            .forecast
            .ok_or(ForecastError::Missing("properties.forecast"))?;
        let body: ForecastResponse = self.get_json(&forecast_url).await?;
        let generated_at = body.properties.generated_at;
        let periods = body
            .properties
            .periods
            .into_iter()
            .map(period_from_raw)
            .collect();
        Ok(Forecast {
            lat,
            lon,
            fetched_at: Utc::now(),
            generated_at,
            periods,
        })
    }

    async fn get_json<T: for<'de> Deserialize<'de>>(&self, url: &str) -> Result<T, ForecastError> {
        let resp = self
            .client
            .get(url)
            .header("User-Agent", &self.user_agent)
            .header("Accept", "application/geo+json")
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(ForecastError::Status {
                status: status.as_u16(),
                url: url.to_string(),
                body,
            });
        }
        let bytes = resp.bytes().await?;
        serde_json::from_slice(&bytes).map_err(Into::into)
    }
}

fn period_from_raw(p: RawPeriod) -> ForecastPeriod {
    let pop = p.probability_of_precipitation.and_then(|v| v.value);
    if p.temperature_unit.as_deref() != Some("F") {
        warn!(
            unit = ?p.temperature_unit,
            "NWS returned non-Fahrenheit temperature; v1 assumes °F"
        );
    }
    ForecastPeriod {
        name: p.name,
        is_daytime: p.is_daytime,
        temperature_f: p.temperature,
        precipitation_probability_pct: pop,
        start_time: p.start_time,
        end_time: p.end_time,
        detailed_forecast: p.detailed_forecast,
    }
}

// ─── Wire-format types (NWS GeoJSON) ───────────────────────────────────────

#[derive(Debug, Deserialize)]
struct PointsResponse {
    properties: PointsProperties,
}

#[derive(Debug, Deserialize)]
struct PointsProperties {
    #[serde(default)]
    forecast: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ForecastResponse {
    properties: ForecastProperties,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ForecastProperties {
    /// When NWS *issued* this forecast. Distinct from when we polled it.
    /// `generatedAt` is the field exposed in the GeoJSON properties block;
    /// older NWS responses use `updateTime` — accept either.
    #[serde(default, alias = "updateTime")]
    generated_at: Option<DateTime<Utc>>,
    periods: Vec<RawPeriod>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawPeriod {
    name: String,
    start_time: DateTime<Utc>,
    end_time: DateTime<Utc>,
    is_daytime: bool,
    temperature: i32,
    #[serde(default)]
    temperature_unit: Option<String>,
    #[serde(default)]
    probability_of_precipitation: Option<RawPop>,
    detailed_forecast: String,
}

#[derive(Debug, Deserialize)]
struct RawPop {
    /// NWS sometimes returns null here.
    #[serde(default)]
    value: Option<i32>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Trimmed real NWS forecast response (NYC, fetched 2026-04-26).
    /// Asserts the deserializer picks up the camelCase fields and the
    /// nullable `probabilityOfPrecipitation.value`.
    const FIXTURE_FORECAST: &str = r#"{
        "properties": {
            "periods": [
                {
                    "name": "Today",
                    "startTime": "2026-04-26T12:00:00+00:00",
                    "endTime": "2026-04-27T00:00:00+00:00",
                    "isDaytime": true,
                    "temperature": 72,
                    "temperatureUnit": "F",
                    "probabilityOfPrecipitation": {"value": 20},
                    "detailedForecast": "Sunny, with a high near 72."
                },
                {
                    "name": "Tonight",
                    "startTime": "2026-04-27T00:00:00+00:00",
                    "endTime": "2026-04-27T12:00:00+00:00",
                    "isDaytime": false,
                    "temperature": 54,
                    "temperatureUnit": "F",
                    "probabilityOfPrecipitation": {"value": null},
                    "detailedForecast": "Mostly clear."
                }
            ]
        }
    }"#;

    #[test]
    fn parses_real_nws_forecast_response() {
        let parsed: ForecastResponse = serde_json::from_str(FIXTURE_FORECAST).unwrap();
        let periods: Vec<ForecastPeriod> = parsed
            .properties
            .periods
            .into_iter()
            .map(period_from_raw)
            .collect();

        assert_eq!(periods.len(), 2);
        assert!(periods[0].is_daytime);
        assert_eq!(periods[0].temperature_f, 72);
        assert_eq!(periods[0].precipitation_probability_pct, Some(20));
        assert!(!periods[1].is_daytime);
        assert_eq!(periods[1].temperature_f, 54);
        // Null PoP must round-trip to None, not 0.
        assert_eq!(periods[1].precipitation_probability_pct, None);
    }
}
