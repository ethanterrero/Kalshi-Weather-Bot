//! NWS METAR / ASOS observation fetcher + running-extreme aggregator.
//!
//! On settlement day, the bot can read the running daily-high (or low)
//! temperature directly from observations and update P(YES) for thin
//! late-day Kalshi markets — by 2pm local most of the daily high is
//! already on the board. This module exposes the raw obs and the
//! aggregate "running extreme so far inside the Kalshi standard-time
//! settlement window."
//!
//! Source: NWS `api.weather.gov` `/stations/{ICAO}/observations` — same
//! host and `User-Agent` convention as `NwsClient`. Response shape is
//! GeoJSON `FeatureCollection`; temperatures arrive in `wmoUnit:degC`
//! and are converted to °F at the parser boundary so callers see only
//! the unit Kalshi settles on.
//!
//! Verified live shape (`/stations/KNYC/observations?limit=2`,
//! 2026-05-17):
//!   `properties.timestamp`      : RFC3339 timestamp (UTC offset).
//!   `properties.temperature`    : `{ unitCode, value, qualityControl }`.
//!   `properties.temperature.value` : `null` when the station reported
//!                                    `M` (missing) for that slot.
//!
//! Quality flags (NWS Codes Registry):
//!   `V` = valid · `C` = coarse-passed QC · `S` = subjective ·
//!   `Z` = preliminary / unchecked.
//! For settlement-grade math we gate on `V | C`. Anything else is
//! dropped from the aggregate but counted so the operator can spot a
//! station whose feed has drifted.
//!
//! See `docs/research/metar-observations.md` for the per-city station
//! verification table (all five v1 cities verified to publish METARs at
//! the same ICAO Kalshi settles against) and the IEM fallback notes.

use chrono::{DateTime, Utc};
use serde::Deserialize;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum MetarError {
    #[error("HTTP: {0}")]
    Http(#[from] reqwest::Error),
    #[error("NWS {status} for {url}: {body}")]
    Status {
        status: u16,
        url: String,
        body: String,
    },
    #[error("JSON parse: {0}")]
    Json(#[from] serde_json::Error),
}

/// NWS temperature-observation quality flag, per the WMO Codes
/// Registry. Settlement-grade computations gate on
/// [`is_settlement_grade`](QualityFlag::is_settlement_grade) so a
/// preliminary or subjective value can't push the running max around.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QualityFlag {
    /// `V` — passed QC.
    Valid,
    /// `C` — coarse-passed QC (range-check only). Treated as good enough
    /// for the settlement-day running extreme; NWS itself uses these for
    /// hourly METAR.
    Coarse,
    /// `S` — subjective, hand-edited. Rare; not used in the aggregate.
    Subjective,
    /// `Z` — preliminary / unchecked. Common on derived fields, very
    /// rare on `temperature` itself.
    Preliminary,
    /// Any other letter NWS introduces later, or an unset flag.
    Unknown,
}

impl QualityFlag {
    pub fn parse(s: Option<&str>) -> Self {
        match s {
            Some("V") => QualityFlag::Valid,
            Some("C") => QualityFlag::Coarse,
            Some("S") => QualityFlag::Subjective,
            Some("Z") => QualityFlag::Preliminary,
            _ => QualityFlag::Unknown,
        }
    }

    /// `true` for flags the running-extreme aggregator trusts.
    pub fn is_settlement_grade(&self) -> bool {
        matches!(self, QualityFlag::Valid | QualityFlag::Coarse)
    }
}

/// One temperature observation, normalized to °F at the boundary. NWS
/// reports temperature as `wmoUnit:degC`; the `parse_observations_json`
/// path performs the °C → °F conversion before constructing this struct
/// so downstream code never sees Celsius.
#[derive(Debug, Clone, PartialEq)]
pub struct MetarObservation {
    pub timestamp: DateTime<Utc>,
    pub temperature_f: f64,
    pub quality: QualityFlag,
}

/// Aggregate over a window — the daily high or low so far inside the
/// Kalshi standard-time settlement window. `n_observations` counts only
/// the settlement-grade temperature obs that fed `value_f` (rejected
/// ones aren't included). `latest_ts` is the most recent settlement-
/// grade timestamp in-window, useful for staleness gating.
#[derive(Debug, Clone, PartialEq)]
pub struct MetarSnapshot {
    pub value_f: f64,
    pub latest_ts: DateTime<Utc>,
    pub n_observations: usize,
}

pub struct MetarClient {
    http: reqwest::Client,
    base_url: String,
    user_agent: String,
}

impl MetarClient {
    pub fn new(base_url: String, user_agent: String) -> Self {
        Self {
            http: reqwest::Client::new(),
            base_url,
            user_agent,
        }
    }

    /// Convenience for the production base URL.
    pub fn nws(user_agent: String) -> Self {
        Self::new("https://api.weather.gov".to_string(), user_agent)
    }

    /// Fetch observations for `[start, end]` (inclusive on both ends in
    /// NWS's interpretation). Returns parsed observations sorted oldest
    /// → newest; the caller is free to aggregate however it likes.
    ///
    /// NWS publishes a routine METAR per hour plus 5-minute ASOS specials
    /// at major airports, so a single-day query returns ~290 features.
    /// No documented rate limit; one request per city per pass is well
    /// inside the "reasonable use" envelope.
    pub async fn fetch_observations(
        &self,
        icao: &str,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> Result<Vec<MetarObservation>, MetarError> {
        // NWS rejects fractional-second timestamps with a parameter error
        // (verified live, 2026-05-17). Format strictly as `…Z` whole-second.
        let url = format!(
            "{}/stations/{}/observations?start={}&end={}",
            self.base_url,
            icao,
            start.format("%Y-%m-%dT%H:%M:%SZ"),
            end.format("%Y-%m-%dT%H:%M:%SZ"),
        );
        let resp = self
            .http
            .get(&url)
            .header("User-Agent", &self.user_agent)
            .header("Accept", "application/geo+json")
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(MetarError::Status {
                status: status.as_u16(),
                url,
                body,
            });
        }
        let bytes = resp.bytes().await?;
        let text = std::str::from_utf8(&bytes).map_err(|e| {
            serde_json::Error::io(std::io::Error::new(std::io::ErrorKind::InvalidData, e))
        })?;
        let mut obs = parse_observations_json(text)?;
        obs.sort_by_key(|o| o.timestamp);
        Ok(obs)
    }
}

/// Parse the GeoJSON `FeatureCollection` NWS returns from
/// `/observations`. °C → °F conversion happens here so every
/// `MetarObservation` downstream is already in Kalshi-native units.
/// Observations with `temperature.value = null` (NWS code `M`/missing)
/// are silently dropped — they carry no information for the running
/// extreme.
pub fn parse_observations_json(payload: &str) -> Result<Vec<MetarObservation>, MetarError> {
    let parsed: FeatureCollection = serde_json::from_str(payload)?;
    let mut out = Vec::with_capacity(parsed.features.len());
    for feat in parsed.features {
        let Some(temp) = feat.properties.temperature else {
            continue;
        };
        let Some(value_c) = temp.value else {
            continue;
        };
        let quality = QualityFlag::parse(temp.quality_control.as_deref());
        out.push(MetarObservation {
            timestamp: feat.properties.timestamp,
            temperature_f: celsius_to_fahrenheit(value_c),
            quality,
        });
    }
    Ok(out)
}

/// Running daily-high inside `[window_start, window_end)`. Filters out
/// non-settlement-grade obs (anything other than `V` / `C`) before
/// taking the max. Returns `None` when no in-window settlement-grade
/// temperature was seen — caller falls back to whatever its baseline
/// estimate was.
///
/// `window_*` should come from
/// `weather_types::daily_high_window_utc(city, date)` so the bot uses
/// the same standard-time window pricing already uses. Half-open in
/// `start`, half-open in `end` to match that helper.
pub fn running_high_f(
    observations: &[MetarObservation],
    window_start: DateTime<Utc>,
    window_end: DateTime<Utc>,
) -> Option<MetarSnapshot> {
    extreme_f(observations, window_start, window_end, true)
}

/// Running daily-low inside the standard-time low window. See
/// [`running_high_f`] for window semantics.
pub fn running_low_f(
    observations: &[MetarObservation],
    window_start: DateTime<Utc>,
    window_end: DateTime<Utc>,
) -> Option<MetarSnapshot> {
    extreme_f(observations, window_start, window_end, false)
}

fn extreme_f(
    observations: &[MetarObservation],
    window_start: DateTime<Utc>,
    window_end: DateTime<Utc>,
    take_max: bool,
) -> Option<MetarSnapshot> {
    let mut extreme: Option<f64> = None;
    let mut latest_ts: Option<DateTime<Utc>> = None;
    let mut n: usize = 0;
    for o in observations {
        if o.timestamp < window_start || o.timestamp >= window_end {
            continue;
        }
        if !o.quality.is_settlement_grade() {
            continue;
        }
        n += 1;
        extreme = Some(match (extreme, take_max) {
            (None, _) => o.temperature_f,
            (Some(prev), true) => prev.max(o.temperature_f),
            (Some(prev), false) => prev.min(o.temperature_f),
        });
        latest_ts = Some(match latest_ts {
            None => o.timestamp,
            Some(prev) => prev.max(o.timestamp),
        });
    }
    let value_f = extreme?;
    let latest_ts = latest_ts?;
    Some(MetarSnapshot {
        value_f,
        latest_ts,
        n_observations: n,
    })
}

fn celsius_to_fahrenheit(c: f64) -> f64 {
    c * 9.0 / 5.0 + 32.0
}

// ─── Wire-format types (GeoJSON FeatureCollection) ───────────────────────

#[derive(Debug, Deserialize)]
struct FeatureCollection {
    features: Vec<Feature>,
}

#[derive(Debug, Deserialize)]
struct Feature {
    properties: FeatureProperties,
}

#[derive(Debug, Deserialize)]
struct FeatureProperties {
    timestamp: DateTime<Utc>,
    #[serde(default)]
    temperature: Option<RawMeasurement>,
}

#[derive(Debug, Deserialize)]
struct RawMeasurement {
    /// NWS returns `null` when the underlying observation was `M`
    /// (missing). We drop those obs entirely; the running-extreme
    /// aggregator wants temperatures, not nulls.
    #[serde(default)]
    value: Option<f64>,
    #[serde(rename = "qualityControl", default)]
    quality_control: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    /// Trimmed real-shape fixture: three KNYC obs across a half-day
    /// stretch. One is QC = `V`, one is `C`, one is `Z` (preliminary)
    /// to exercise the quality gate.
    const FIXTURE_KNYC_3OBS: &str = r#"{
        "type": "FeatureCollection",
        "features": [
            {
                "properties": {
                    "timestamp": "2026-07-04T12:51:00+00:00",
                    "temperature": {"unitCode": "wmoUnit:degC", "value": 22.0, "qualityControl": "V"}
                }
            },
            {
                "properties": {
                    "timestamp": "2026-07-04T18:51:00+00:00",
                    "temperature": {"unitCode": "wmoUnit:degC", "value": 32.0, "qualityControl": "C"}
                }
            },
            {
                "properties": {
                    "timestamp": "2026-07-04T19:51:00+00:00",
                    "temperature": {"unitCode": "wmoUnit:degC", "value": 34.0, "qualityControl": "Z"}
                }
            },
            {
                "properties": {
                    "timestamp": "2026-07-04T20:51:00+00:00",
                    "temperature": {"unitCode": "wmoUnit:degC", "value": null, "qualityControl": "V"}
                }
            }
        ]
    }"#;

    fn dt(y: i32, m: u32, d: u32, h: u32, mi: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, m, d, h, mi, 0).unwrap()
    }

    #[test]
    fn parses_real_nws_observations_shape_and_converts_to_fahrenheit() {
        let obs = parse_observations_json(FIXTURE_KNYC_3OBS).unwrap();
        // 4 features total, but one is null-temperature → dropped. 3 left.
        assert_eq!(obs.len(), 3);
        // 22.0°C → 71.6°F.
        assert!(
            (obs[0].temperature_f - 71.6).abs() < 1e-6,
            "{}",
            obs[0].temperature_f
        );
        assert_eq!(obs[0].quality, QualityFlag::Valid);
        assert_eq!(obs[1].quality, QualityFlag::Coarse);
        assert_eq!(obs[2].quality, QualityFlag::Preliminary);
    }

    #[test]
    fn null_temperature_value_is_dropped_not_zero() {
        // The fourth fixture row has `value: null`. It must not appear
        // in the parsed output (otherwise the running min would dip to 32°F).
        let obs = parse_observations_json(FIXTURE_KNYC_3OBS).unwrap();
        for o in &obs {
            // No observation should be the "32°F" sentinel that comes from
            // mis-parsing null → 0 °C.
            assert!((o.temperature_f - 32.0).abs() > 1e-6 || o.quality != QualityFlag::Valid);
        }
    }

    #[test]
    fn running_high_uses_settlement_grade_only_and_returns_max() {
        let obs = parse_observations_json(FIXTURE_KNYC_3OBS).unwrap();
        let start = dt(2026, 7, 4, 0, 0);
        let end = dt(2026, 7, 5, 0, 0);
        let snap = running_high_f(&obs, start, end).unwrap();
        // The preliminary `Z` obs at 34°C must be ignored; max settles
        // on the 32°C / `C` obs → 89.6°F.
        assert!((snap.value_f - 89.6).abs() < 1e-6, "{}", snap.value_f);
        // n_observations counts settlement-grade in-window only (V + C).
        assert_eq!(snap.n_observations, 2);
        assert_eq!(snap.latest_ts, dt(2026, 7, 4, 18, 51));
    }

    #[test]
    fn running_low_picks_min_in_window() {
        let obs = parse_observations_json(FIXTURE_KNYC_3OBS).unwrap();
        let start = dt(2026, 7, 4, 0, 0);
        let end = dt(2026, 7, 5, 0, 0);
        let snap = running_low_f(&obs, start, end).unwrap();
        // 22°C → 71.6°F is the lowest settlement-grade in-window obs.
        assert!((snap.value_f - 71.6).abs() < 1e-6, "{}", snap.value_f);
    }

    #[test]
    fn out_of_window_observations_are_ignored() {
        let obs = parse_observations_json(FIXTURE_KNYC_3OBS).unwrap();
        // Window starts after all the fixture timestamps.
        let start = dt(2026, 7, 5, 0, 0);
        let end = dt(2026, 7, 6, 0, 0);
        assert!(running_high_f(&obs, start, end).is_none());
        assert!(running_low_f(&obs, start, end).is_none());
    }

    #[test]
    fn all_non_settlement_grade_returns_none() {
        // Synthesise a payload of one `Z`-flagged obs only — caller
        // should get None back, *not* a value derived from preliminary data.
        let payload = r#"{
            "features": [
                {"properties": {
                    "timestamp": "2026-07-04T18:51:00+00:00",
                    "temperature": {"unitCode": "wmoUnit:degC", "value": 30.0, "qualityControl": "Z"}
                }}
            ]
        }"#;
        let obs = parse_observations_json(payload).unwrap();
        let start = dt(2026, 7, 4, 0, 0);
        let end = dt(2026, 7, 5, 0, 0);
        assert!(running_high_f(&obs, start, end).is_none());
    }

    #[test]
    fn quality_flag_parses_known_letters_and_falls_back_to_unknown() {
        assert_eq!(QualityFlag::parse(Some("V")), QualityFlag::Valid);
        assert_eq!(QualityFlag::parse(Some("C")), QualityFlag::Coarse);
        assert_eq!(QualityFlag::parse(Some("S")), QualityFlag::Subjective);
        assert_eq!(QualityFlag::parse(Some("Z")), QualityFlag::Preliminary);
        assert_eq!(QualityFlag::parse(Some("Q")), QualityFlag::Unknown);
        assert_eq!(QualityFlag::parse(None), QualityFlag::Unknown);
        assert!(QualityFlag::Valid.is_settlement_grade());
        assert!(QualityFlag::Coarse.is_settlement_grade());
        assert!(!QualityFlag::Subjective.is_settlement_grade());
        assert!(!QualityFlag::Preliminary.is_settlement_grade());
    }

    #[test]
    fn malformed_payload_is_an_error() {
        assert!(parse_observations_json("not json").is_err());
        // Wrong shape — missing `features`.
        assert!(parse_observations_json("{}").is_err());
    }

    /// Live integration test against api.weather.gov. Skipped by default
    /// to keep `cargo test` offline. Run with:
    ///   `cargo test -p weather-forecast -- --ignored metar::live`
    #[tokio::test]
    #[ignore]
    async fn metar_live_fetch_knyc_recent_returns_some_observations() {
        let client = MetarClient::nws("Kalshi-Weather-Bot-test (ethan.terrero@gmail.com)".into());
        let end = Utc::now();
        let start = end - chrono::Duration::hours(6);
        let obs = client.fetch_observations("KNYC", start, end).await.unwrap();
        // KNYC publishes a routine METAR every hour; over 6h we should see
        // at least 3 settlement-grade obs even if a couple are missing.
        let n_good = obs
            .iter()
            .filter(|o| o.quality.is_settlement_grade())
            .count();
        assert!(
            n_good >= 3,
            "expected >=3 settlement-grade obs in last 6h, got {} of {}",
            n_good,
            obs.len()
        );
    }
}
