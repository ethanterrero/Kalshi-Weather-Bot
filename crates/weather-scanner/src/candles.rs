//! Kalshi historical candlesticks for a single market.
//!
//! Endpoint (verified live against demo-api 2026-05-04):
//!
//!   GET /trade-api/v2/series/{series_ticker}/markets/{ticker}/candlesticks
//!     ?start_ts=<unix>&end_ts=<unix>&period_interval=<1|60|1440>
//!
//! Each candlestick covers a fixed interval (1 / 60 / 1440 minutes) and
//! carries three OHLC blocks:
//!
//!   - `price` — last-trade OHLC. Empty (`{}`) when no trades fired in
//!     that period; that's our `Option<TradeOhlc>`.
//!   - `yes_ask` — top-of-book ask OHLC over the period. Always populated.
//!   - `yes_bid` — top-of-book bid OHLC over the period. Always populated.
//!
//! Kalshi only serves recent candles via this path; markets that settled
//! before the historical cutoff need
//! `GET /historical/markets/{ticker}/candlesticks`. That separate endpoint
//! is out of scope until we hit a market we can't reach via the live path.
//!
//! What this is for: backtest P&L. Joining the JSONL decision log to
//! `yes_ask.close_dollars` (and `yes_bid.close_dollars` for the NO side)
//! at the period containing the decision's `ts` lets us estimate "would
//! this trade have been profitable at the close fill we'd actually see
//! if we placed a post-only order?" It's an approximation — actual fills
//! depend on queue position — but it's the closest thing to ground-truth
//! P&L without recording our own orderbook snapshots going forward.

use rust_decimal::Decimal;
use serde::Deserialize;

use crate::ScannerError;

/// Candle period in minutes. Kalshi accepts only these three values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeriodInterval {
    OneMinute,
    OneHour,
    OneDay,
}

impl PeriodInterval {
    pub fn as_minutes(self) -> u32 {
        match self {
            PeriodInterval::OneMinute => 1,
            PeriodInterval::OneHour => 60,
            PeriodInterval::OneDay => 1440,
        }
    }
}

/// One candlestick. `end_period_ts` is Unix seconds at the end of the
/// period (Kalshi's convention — period is `[end_ts - interval, end_ts)`).
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct Candlestick {
    pub end_period_ts: i64,
    /// Last-trade OHLC, or `None` when no trades happened in this period.
    /// Kalshi's wire format encodes the empty case as `{}`, which serde
    /// can't distinguish from "all-fields-null"; we collapse both into
    /// `None` via a custom converter.
    #[serde(deserialize_with = "deserialize_optional_ohlc")]
    pub price: Option<TradeOhlc>,
    /// Top-of-book ask OHLC. Always populated by Kalshi.
    pub yes_ask: BookOhlc,
    /// Top-of-book bid OHLC.
    pub yes_bid: BookOhlc,
    /// Volume in dollars over the period (Kalshi encodes as a string).
    #[serde(rename = "volume_fp", with = "rust_decimal::serde::str")]
    pub volume_dollars: Decimal,
    /// Open interest in dollars at the end of the period.
    #[serde(rename = "open_interest_fp", with = "rust_decimal::serde::str")]
    pub open_interest_dollars: Decimal,
}

/// OHLC for last-trade prices. Includes `mean` (volume-weighted average)
/// and an optional `previous_dollars` (close price of the prior candle,
/// only present when prepend-previous is enabled or the prior candle
/// existed).
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct TradeOhlc {
    #[serde(
        rename = "open_dollars",
        with = "rust_decimal::serde::str_option",
        default
    )]
    pub open: Option<Decimal>,
    #[serde(
        rename = "high_dollars",
        with = "rust_decimal::serde::str_option",
        default
    )]
    pub high: Option<Decimal>,
    #[serde(
        rename = "low_dollars",
        with = "rust_decimal::serde::str_option",
        default
    )]
    pub low: Option<Decimal>,
    #[serde(
        rename = "close_dollars",
        with = "rust_decimal::serde::str_option",
        default
    )]
    pub close: Option<Decimal>,
    #[serde(
        rename = "mean_dollars",
        with = "rust_decimal::serde::str_option",
        default
    )]
    pub mean: Option<Decimal>,
    #[serde(
        rename = "previous_dollars",
        with = "rust_decimal::serde::str_option",
        default
    )]
    pub previous: Option<Decimal>,
}

/// OHLC for top-of-book ask or bid. Kalshi always populates these even
/// when `price` is empty (the book exists; just nobody traded).
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct BookOhlc {
    #[serde(rename = "open_dollars", with = "rust_decimal::serde::str")]
    pub open: Decimal,
    #[serde(rename = "high_dollars", with = "rust_decimal::serde::str")]
    pub high: Decimal,
    #[serde(rename = "low_dollars", with = "rust_decimal::serde::str")]
    pub low: Decimal,
    #[serde(rename = "close_dollars", with = "rust_decimal::serde::str")]
    pub close: Decimal,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CandlesticksResponse {
    pub ticker: String,
    pub candlesticks: Vec<Candlestick>,
}

/// Convert Kalshi's "empty object means no trades" wire shape into
/// `Option<TradeOhlc>`. An entirely empty object (`{}`) deserialises into
/// a `TradeOhlc` whose every field is `None`; we collapse that into
/// `None` so consumers can do an `if let Some(price) = candle.price`.
fn deserialize_optional_ohlc<'de, D>(deserializer: D) -> Result<Option<TradeOhlc>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = Option::<TradeOhlc>::deserialize(deserializer)?;
    Ok(raw.and_then(|o| {
        if o.open.is_none()
            && o.high.is_none()
            && o.low.is_none()
            && o.close.is_none()
            && o.mean.is_none()
            && o.previous.is_none()
        {
            None
        } else {
            Some(o)
        }
    }))
}

impl crate::KalshiClient {
    /// Fetch candlesticks for a single market. Kalshi requires `start_ts`
    /// and `end_ts` (inclusive Unix seconds) and a `period_interval` of
    /// 1 / 60 / 1440 minutes. The backstop is whichever Kalshi treats as
    /// the historical cutoff for the live endpoint — markets older than
    /// that need `/historical/...` (out of scope).
    pub async fn fetch_candlesticks(
        &self,
        series_ticker: &str,
        ticker: &str,
        start_ts: i64,
        end_ts: i64,
        interval: PeriodInterval,
    ) -> Result<Vec<Candlestick>, ScannerError> {
        let url = format!(
            "{}/series/{}/markets/{}/candlesticks?start_ts={}&end_ts={}&period_interval={}",
            self.base_url(),
            series_ticker,
            ticker,
            start_ts,
            end_ts,
            interval.as_minutes()
        );
        let bytes = self.get_with_retry_pub(&url).await?;
        let resp: CandlesticksResponse = serde_json::from_slice(&bytes)?;
        Ok(resp.candlesticks)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    /// Trimmed real demo-api response (KXHIGHNY-26MAY02-T65, daily
    /// candles, 2026-05-02 + 2026-05-03). The first candle has empty
    /// `price` (no trades that day per the live capture); the second has
    /// real OHLC including `mean_dollars` and `previous_dollars`.
    const FIXTURE_REAL: &str = r#"{
        "ticker": "KXHIGHNY-26MAY02-T65",
        "candlesticks": [
            {
                "end_period_ts": 1777867200,
                "open_interest_fp": "0.00",
                "price": {},
                "volume_fp": "0.00",
                "yes_ask": {
                    "close_dollars": "1.0000",
                    "high_dollars": "1.0000",
                    "low_dollars": "0.0100",
                    "open_dollars": "0.1200"
                },
                "yes_bid": {
                    "close_dollars": "0.0800",
                    "high_dollars": "0.0800",
                    "low_dollars": "0.0000",
                    "open_dollars": "0.0000"
                }
            },
            {
                "end_period_ts": 1777953600,
                "open_interest_fp": "181.00",
                "price": {
                    "close_dollars": "0.0400",
                    "high_dollars": "0.0800",
                    "low_dollars": "0.0100",
                    "mean_dollars": "0.0341",
                    "open_dollars": "0.0100",
                    "previous_dollars": "0.5000"
                },
                "volume_fp": "22.00",
                "yes_ask": {
                    "close_dollars": "0.0800",
                    "high_dollars": "0.5000",
                    "low_dollars": "0.0100",
                    "open_dollars": "0.5000"
                },
                "yes_bid": {
                    "close_dollars": "0.0400",
                    "high_dollars": "0.0400",
                    "low_dollars": "0.0000",
                    "open_dollars": "0.0000"
                }
            }
        ]
    }"#;

    #[test]
    fn parses_real_demo_api_shape_with_mixed_traded_and_untraded_periods() {
        let resp: CandlesticksResponse = serde_json::from_str(FIXTURE_REAL).unwrap();
        assert_eq!(resp.ticker, "KXHIGHNY-26MAY02-T65");
        assert_eq!(resp.candlesticks.len(), 2);

        let no_trades = &resp.candlesticks[0];
        assert_eq!(no_trades.end_period_ts, 1777867200);
        assert!(no_trades.price.is_none(), "empty {{}} → None");
        assert_eq!(no_trades.volume_dollars, dec!(0));
        assert_eq!(no_trades.yes_ask.close, dec!(1.0));
        assert_eq!(no_trades.yes_bid.close, dec!(0.08));

        let traded = &resp.candlesticks[1];
        let p = traded.price.as_ref().expect("trades happened");
        assert_eq!(p.close, Some(dec!(0.04)));
        assert_eq!(p.mean, Some(dec!(0.0341)));
        assert_eq!(p.previous, Some(dec!(0.50)));
        assert_eq!(traded.volume_dollars, dec!(22));
        assert_eq!(traded.open_interest_dollars, dec!(181));
    }

    #[test]
    fn period_interval_minutes_match_kalshi_spec() {
        assert_eq!(PeriodInterval::OneMinute.as_minutes(), 1);
        assert_eq!(PeriodInterval::OneHour.as_minutes(), 60);
        assert_eq!(PeriodInterval::OneDay.as_minutes(), 1440);
    }

    #[test]
    fn empty_candlesticks_array_round_trips() {
        let payload = r#"{"ticker":"X","candlesticks":[]}"#;
        let resp: CandlesticksResponse = serde_json::from_str(payload).unwrap();
        assert_eq!(resp.ticker, "X");
        assert!(resp.candlesticks.is_empty());
    }

    /// Live integration test against demo-api. Skipped by default. Run:
    ///   `cargo test -p weather-scanner -- --ignored candles::live`
    #[tokio::test]
    #[ignore]
    async fn candles_live_fetch_recent_kxhighny_market() {
        let client = crate::KalshiClient::new("https://demo-api.kalshi.co/trade-api/v2".into());
        // Find a real recent market to query.
        let raws = client
            .list_open_markets_in_series("KXHIGHNY")
            .await
            .unwrap();
        let raw = raws.first().expect("at least one open KXHIGHNY market");
        let now = chrono::Utc::now().timestamp();
        let start = now - 86400 * 7;
        let candles = client
            .fetch_candlesticks("KXHIGHNY", &raw.ticker, start, now, PeriodInterval::OneDay)
            .await
            .unwrap();
        // We don't assert on count — a brand-new market may have zero
        // candles. We only assert the shape parsed.
        for c in &candles {
            assert!(c.end_period_ts >= start);
            assert!(c.end_period_ts <= now);
        }
    }
}
