//! Kalshi market scanner — discovers tradeable weather markets and parses
//! their tickers into structured `WeatherThreshold`s.
//!
//! Wire format pinned against demo-api.kalshi.co responses captured
//! 2026-04-26. Kalshi's `/markets` endpoint is public (no auth), paginates
//! via a `cursor` field, and accepts `series_ticker=...&status=open`.

use chrono::{DateTime, NaiveDate, Utc};
use rust_decimal::Decimal;
use serde::Deserialize;
use std::sync::Arc;
use thiserror::Error;
use tokio::sync::RwLock;
use tracing::{debug, info, warn};
use weather_config::AppConfig;
use weather_types::{KalshiMarket, TempStat, ThresholdDirection, WeatherThreshold};

#[derive(Debug, Error)]
pub enum ScannerError {
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("Kalshi {status} for {url}: {body}")]
    Status {
        status: u16,
        url: String,
        body: String,
    },
    #[error("JSON parse error: {0}")]
    Json(#[from] serde_json::Error),
}

/// One (market, parsed-threshold) pair the scanner has discovered.
#[derive(Debug, Clone)]
pub struct TrackedMarket {
    pub market: KalshiMarket,
    pub threshold: WeatherThreshold,
}

pub struct KalshiClient {
    http: reqwest::Client,
    base_url: String,
}

impl KalshiClient {
    pub fn new(base_url: String) -> Self {
        Self {
            http: reqwest::Client::new(),
            base_url,
        }
    }

    /// Fetch all open markets for `series_ticker`, auto-paginating through
    /// the cursor field until Kalshi returns an empty cursor.
    pub async fn list_open_markets_in_series(
        &self,
        series_ticker: &str,
    ) -> Result<Vec<RawMarket>, ScannerError> {
        let mut out = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let mut url = format!(
                "{}/markets?series_ticker={}&status=open&limit=200",
                self.base_url, series_ticker
            );
            if let Some(c) = &cursor {
                url.push_str("&cursor=");
                url.push_str(c);
            }
            let resp = self.http.get(&url).send().await?;
            let status = resp.status();
            if !status.is_success() {
                let body = resp.text().await.unwrap_or_default();
                return Err(ScannerError::Status {
                    status: status.as_u16(),
                    url,
                    body,
                });
            }
            let bytes = resp.bytes().await?;
            let page: MarketsResponse = serde_json::from_slice(&bytes)?;
            out.extend(page.markets);
            // Kalshi returns a cursor even on the last page; the convention is
            // that the response is empty when there's nothing left.
            match page.cursor {
                Some(c) if !c.is_empty() && !out.is_empty() && out.len() % 200 == 0 => {
                    cursor = Some(c);
                }
                _ => break,
            }
        }
        Ok(out)
    }
}

pub struct MarketScanner {
    client: KalshiClient,
    series_tickers: Vec<String>,
    state: Arc<RwLock<Vec<TrackedMarket>>>,
}

impl MarketScanner {
    pub fn new(config: &AppConfig) -> Self {
        Self {
            client: KalshiClient::new(config.kalshi.base_url().to_string()),
            series_tickers: config.scanner.series_tickers.clone(),
            state: Arc::new(RwLock::new(Vec::new())),
        }
    }

    /// Hit Kalshi for every configured series, parse each market's ticker
    /// into a `WeatherThreshold`, and store the union. Returns the count of
    /// tracked markets.
    pub async fn refresh(&self) -> Result<usize, ScannerError> {
        let mut tracked = Vec::new();
        for series in &self.series_tickers {
            let raws = match self.client.list_open_markets_in_series(series).await {
                Ok(v) => v,
                Err(e) => {
                    warn!(series, error = %e, "series fetch failed; skipping");
                    continue;
                }
            };
            let before = tracked.len();
            for raw in raws {
                match (parse_weather_threshold(&raw.ticker, &raw.strike_type), raw.into_market()) {
                    (Some(threshold), market) => tracked.push(TrackedMarket { market, threshold }),
                    _ => {} // unsupported shape (between-bins, custom strikes) — silently skip
                }
            }
            debug!(series, parsed = tracked.len() - before, "series scan complete");
        }
        let count = tracked.len();
        *self.state.write().await = tracked;
        info!(count, "Market refresh complete");
        Ok(count)
    }

    pub fn markets(&self) -> Arc<RwLock<Vec<TrackedMarket>>> {
        self.state.clone()
    }
}

// ─── Ticker parser ─────────────────────────────────────────────────────────

/// Parse a Kalshi temperature high/low market ticker + strike type into a
/// `WeatherThreshold`. Returns None for unsupported shapes:
///   - between-bins (`-B<n>.5`, `strike_type=between`)
///   - custom-strike multi-leg events
///   - non-temperature series
///
/// Examples (real, from demo-api 2026-04-26):
///   - "KXHIGHNY-26APR27-T69" + "greater" → high ≥ 70°F
///   - "KXHIGHNY-26APR27-T62" + "less"    → high ≤ 61°F
///   - "KXLOWLAX-26APR27-T55" + "greater" → low  ≥ 56°F
pub fn parse_weather_threshold(ticker: &str, strike_type: &str) -> Option<WeatherThreshold> {
    // Layout: <series_ticker>-<YYMMDD>-<strike_part>
    let mut parts = ticker.splitn(3, '-');
    let series = parts.next()?;
    let date_part = parts.next()?;
    let strike_part = parts.next()?;

    let (stat, city) = parse_series(series)?;
    let date = parse_kalshi_date(date_part)?;

    let strike_part = strike_part.strip_prefix('T')?; // bins (B...) and custom strikes return None.
    let n: i32 = strike_part.parse().ok()?;
    let direction = match strike_type {
        "greater" => ThresholdDirection::AtOrAbove,
        "less" => ThresholdDirection::AtOrBelow,
        _ => return None,
    };
    // Kalshi's "T<N> + greater" reads as "high > N", which on integer
    // Fahrenheit is "≥ N+1". Inverse for "less".
    let temperature_f = match direction {
        ThresholdDirection::AtOrAbove => n + 1,
        ThresholdDirection::AtOrBelow => n - 1,
    };

    Some(WeatherThreshold {
        city,
        stat,
        direction,
        temperature_f,
        date,
    })
}

fn parse_series(series: &str) -> Option<(TempStat, String)> {
    if let Some(city) = series.strip_prefix("KXHIGH") {
        Some((TempStat::DailyHigh, city.to_string()))
    } else if let Some(city) = series.strip_prefix("KXLOW") {
        Some((TempStat::DailyLow, city.to_string()))
    } else {
        None
    }
}

/// "26APR27" → 2026-04-27. Kalshi uses two-digit year + uppercase 3-letter
/// month + two-digit day.
fn parse_kalshi_date(s: &str) -> Option<NaiveDate> {
    if s.len() != 7 {
        return None;
    }
    let yy: i32 = s[0..2].parse().ok()?;
    let mon = match &s[2..5] {
        "JAN" => 1, "FEB" => 2, "MAR" => 3, "APR" => 4, "MAY" => 5, "JUN" => 6,
        "JUL" => 7, "AUG" => 8, "SEP" => 9, "OCT" => 10, "NOV" => 11, "DEC" => 12,
        _ => return None,
    };
    let day: u32 = s[5..7].parse().ok()?;
    NaiveDate::from_ymd_opt(2000 + yy, mon, day)
}

// ─── Wire format ───────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct MarketsResponse {
    #[serde(default)]
    cursor: Option<String>,
    #[serde(default)]
    markets: Vec<RawMarket>,
}

/// Subset of fields we consume from Kalshi's `/markets` response.
/// Prices and volume come back as JSON strings (e.g. `"0.4500"`); we use
/// `Option` to distinguish missing fields from explicit zero.
#[derive(Debug, Deserialize)]
pub struct RawMarket {
    pub ticker: String,
    pub event_ticker: String,
    pub title: String,
    pub yes_sub_title: String,
    pub close_time: DateTime<Utc>,
    pub status: String,
    pub strike_type: String,

    #[serde(default, with = "rust_decimal::serde::str_option")]
    pub yes_ask_dollars: Option<Decimal>,
    #[serde(default, with = "rust_decimal::serde::str_option")]
    pub yes_bid_dollars: Option<Decimal>,
    #[serde(default, with = "rust_decimal::serde::str_option")]
    pub last_price_dollars: Option<Decimal>,
    #[serde(default, with = "rust_decimal::serde::str_option")]
    pub volume_fp: Option<Decimal>,
    #[serde(default, with = "rust_decimal::serde::str_option")]
    pub open_interest_fp: Option<Decimal>,
}

impl RawMarket {
    fn into_market(self) -> KalshiMarket {
        // event_ticker has the form "KXHIGHNY-26APR27"; strip the date suffix
        // to get the series ticker. Kalshi doesn't return series_ticker on
        // markets directly.
        let series_ticker = self
            .event_ticker
            .rsplit_once('-')
            .map(|(s, _)| s.to_string())
            .unwrap_or_else(|| self.event_ticker.clone());
        // resolution_date == close_time's calendar day in UTC. Good enough for
        // v1; markets always close on the day the weather event happens.
        let resolution_date = self.close_time.date_naive();
        KalshiMarket {
            ticker: self.ticker,
            series_ticker,
            event_ticker: self.event_ticker,
            title: self.title,
            yes_sub_title: self.yes_sub_title,
            resolution_date,
            close_time: self.close_time,
            yes_ask: self.yes_ask_dollars,
            yes_bid: self.yes_bid_dollars,
            last_price: self.last_price_dollars,
            volume: self.volume_fp.unwrap_or(Decimal::ZERO),
            open_interest: self.open_interest_fp.unwrap_or(Decimal::ZERO),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Trimmed real demo-api.kalshi.co response captured 2026-04-26
    /// (KXHIGHNY-26APR27 series). Asserts the wire-format deserializer
    /// picks up Kalshi's actual field names — including the `_dollars` /
    /// `_fp` suffixes and the JSON-string number encoding for prices.
    const FIXTURE_MARKETS: &str = r#"{
      "cursor": "CgsIjtK3zwYQiL6SLxIWS1hISUdITlktMjZBUFIyNy1CNjQuNQ",
      "markets": [
        {
          "ticker": "KXHIGHNY-26APR27-T69",
          "event_ticker": "KXHIGHNY-26APR27",
          "title": "Highest temperature in NYC for Apr 27, 2026?",
          "yes_sub_title": "70° or above",
          "close_time": "2026-04-28T04:59:00Z",
          "status": "active",
          "strike_type": "greater",
          "yes_ask_dollars": "0.1700",
          "yes_bid_dollars": "0.0500",
          "last_price_dollars": "0.1600",
          "volume_fp": "11.00",
          "open_interest_fp": "11.00"
        },
        {
          "ticker": "KXHIGHNY-26APR27-T62",
          "event_ticker": "KXHIGHNY-26APR27",
          "title": "Highest temperature in NYC for Apr 27, 2026?",
          "yes_sub_title": "61° or below",
          "close_time": "2026-04-28T04:59:00Z",
          "status": "active",
          "strike_type": "less",
          "yes_ask_dollars": "0.1500",
          "yes_bid_dollars": "0.0400",
          "last_price_dollars": "0.0500",
          "volume_fp": "5.00",
          "open_interest_fp": "5.00"
        },
        {
          "ticker": "KXHIGHNY-26APR27-B64.5",
          "event_ticker": "KXHIGHNY-26APR27",
          "title": "Highest temperature in NYC for Apr 27, 2026?",
          "yes_sub_title": "64° to 65°",
          "close_time": "2026-04-28T04:59:00Z",
          "status": "active",
          "strike_type": "between",
          "yes_ask_dollars": "0.2400",
          "yes_bid_dollars": "0.1800",
          "last_price_dollars": "0.2000",
          "volume_fp": "20.00",
          "open_interest_fp": "20.00"
        }
      ]
    }"#;

    #[test]
    fn deserializes_real_kalshi_markets_response() {
        let parsed: MarketsResponse = serde_json::from_str(FIXTURE_MARKETS).unwrap();
        assert_eq!(parsed.markets.len(), 3);
        let m = &parsed.markets[0];
        assert_eq!(m.ticker, "KXHIGHNY-26APR27-T69");
        assert_eq!(m.strike_type, "greater");
        assert_eq!(m.yes_ask_dollars.unwrap().to_string(), "0.1700");
        assert_eq!(m.volume_fp.unwrap().to_string(), "11.00");
    }

    #[test]
    fn into_market_derives_series_and_resolution_date() {
        let parsed: MarketsResponse = serde_json::from_str(FIXTURE_MARKETS).unwrap();
        let raw = parsed.markets.into_iter().next().unwrap();
        let m = raw.into_market();
        assert_eq!(m.series_ticker, "KXHIGHNY");
        assert_eq!(m.event_ticker, "KXHIGHNY-26APR27");
        assert_eq!(
            m.resolution_date,
            NaiveDate::from_ymd_opt(2026, 4, 28).unwrap(),
            "close_time is 2026-04-28T04:59:00Z, so the UTC calendar day is the 28th"
        );
    }

    #[test]
    fn parses_t_greater_ticker() {
        // KXHIGHNY-26APR27-T69 / "greater" → high ≥ 70°F (Kalshi semantics:
        // "T69 + greater" means high > 69, integer-rounded to ≥ 70).
        let t = parse_weather_threshold("KXHIGHNY-26APR27-T69", "greater").unwrap();
        assert_eq!(t.city, "NY");
        assert_eq!(t.stat, TempStat::DailyHigh);
        assert_eq!(t.direction, ThresholdDirection::AtOrAbove);
        assert_eq!(t.temperature_f, 70);
        assert_eq!(t.date, NaiveDate::from_ymd_opt(2026, 4, 27).unwrap());
    }

    #[test]
    fn parses_t_less_ticker() {
        // KXHIGHNY-26APR27-T62 / "less" → high ≤ 61°F.
        let t = parse_weather_threshold("KXHIGHNY-26APR27-T62", "less").unwrap();
        assert_eq!(t.direction, ThresholdDirection::AtOrBelow);
        assert_eq!(t.temperature_f, 61);
    }

    #[test]
    fn parses_kxlow_series() {
        let t = parse_weather_threshold("KXLOWLAX-26JUL04-T55", "greater").unwrap();
        assert_eq!(t.city, "LAX");
        assert_eq!(t.stat, TempStat::DailyLow);
        assert_eq!(t.temperature_f, 56);
    }

    #[test]
    fn skips_between_bin_markets_for_v1() {
        // B-prefix bins are valid Kalshi shapes but not yet modeled. They
        // should return None, not panic — scanner silently skips.
        assert!(parse_weather_threshold("KXHIGHNY-26APR27-B64.5", "between").is_none());
    }

    #[test]
    fn skips_unknown_strike_types() {
        // Custom multi-leg events have strike_type="custom"; not weather.
        assert!(parse_weather_threshold("KXHIGHNY-26APR27-T69", "custom").is_none());
    }

    #[test]
    fn skips_non_weather_series() {
        // KXEPLGAME etc. shouldn't parse as weather even though the layout
        // is similar.
        assert!(parse_weather_threshold("KXEPLGAME-26MAY02-T1", "greater").is_none());
    }

    /// Live integration smoke test against demo-api.kalshi.co. Marked
    /// `#[ignore]` so default `cargo test` stays offline; run with
    /// `cargo test -p weather-scanner -- --ignored` when you want to
    /// verify the live endpoint hasn't changed shape.
    #[tokio::test]
    #[ignore]
    async fn smoke_test_demo_api_kxhighny() {
        let client = KalshiClient::new(
            "https://demo-api.kalshi.co/trade-api/v2".to_string(),
        );
        let raws = client
            .list_open_markets_in_series("KXHIGHNY")
            .await
            .expect("demo-api fetch");
        assert!(!raws.is_empty(), "expected at least one open KXHIGHNY market");
        let parsed: Vec<_> = raws
            .iter()
            .filter_map(|r| parse_weather_threshold(&r.ticker, &r.strike_type))
            .collect();
        assert!(
            !parsed.is_empty(),
            "expected at least one ticker to parse as a weather threshold"
        );
        println!("got {} markets, {} parsed", raws.len(), parsed.len());
    }

    #[test]
    fn parses_kalshi_date_format() {
        assert_eq!(
            parse_kalshi_date("26APR27"),
            Some(NaiveDate::from_ymd_opt(2026, 4, 27).unwrap())
        );
        assert_eq!(
            parse_kalshi_date("26DEC31"),
            Some(NaiveDate::from_ymd_opt(2026, 12, 31).unwrap())
        );
        assert_eq!(parse_kalshi_date("26ZZZ27"), None); // bad month
        assert_eq!(parse_kalshi_date("short"), None);
    }
}
