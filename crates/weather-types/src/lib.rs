//! Shared domain types for the Kalshi weather trading bot.
//!
//! Kalshi-specific naming: a "market" is a single binary contract (e.g. "NYC
//! high ≥ 75°F on 2026-07-04"). Markets are grouped under "events" (the
//! underlying real-world question, e.g. "NYC high on 2026-07-04") which are
//! grouped under "series" (the recurring contract template, e.g. "KXHIGH-NY").
//! Threshold values live on the market, not the event.

pub mod cities;
pub mod tempwindow;

pub use cities::{lookup_city, CitySpec};
pub use tempwindow::{daily_high_window_utc, daily_low_window_utc};

use chrono::{DateTime, NaiveDate, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// A single binary contract listed on Kalshi.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KalshiMarket {
    /// Per-market unique ticker (e.g. "KXHIGH-NY-26JUL04-T75").
    pub ticker: String,
    /// The recurring series this market belongs to (e.g. "KXHIGH-NY").
    pub series_ticker: String,
    /// The event this market resolves under (e.g. "KXHIGH-NY-26JUL04").
    pub event_ticker: String,
    /// Human-readable title shown in the Kalshi UI.
    pub title: String,
    /// What "YES" means in plain English ("≥ 75°F", etc.).
    pub yes_sub_title: String,
    /// Resolution date (the day the underlying weather event occurs).
    pub resolution_date: NaiveDate,
    /// Market closes for trading at this UTC instant.
    pub close_time: DateTime<Utc>,
    /// Best ask price for YES, in dollars (0.0–1.0). None if no asks resting.
    pub yes_ask: Option<Decimal>,
    /// Best bid price for YES, in dollars.
    pub yes_bid: Option<Decimal>,
    /// Most recent trade price for YES, in dollars.
    pub last_price: Option<Decimal>,
    /// Total volume traded so far, in dollars.
    pub volume: Decimal,
    /// Open interest (notional resting), in dollars.
    pub open_interest: Decimal,
}

/// Threshold-shaped weather contract details parsed out of a `KalshiMarket`.
/// Examples: high temp ≥ 75°F, low temp ≤ 30°F. v1 only handles temperature.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WeatherThreshold {
    /// City code as Kalshi uses it ("NY", "CHI", "LA", ...).
    pub city: String,
    /// Whether this is a daily HIGH or daily LOW market.
    pub stat: TempStat,
    /// Threshold direction: YES wins if observed temp is ≥ or ≤ `temperature_f`.
    pub direction: ThresholdDirection,
    pub temperature_f: i32,
    pub date: NaiveDate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TempStat {
    DailyHigh,
    DailyLow,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThresholdDirection {
    AtOrAbove,
    AtOrBelow,
}

/// A point-forecast snapshot pulled from NOAA NWS for a specific lat/lon.
/// Source of truth for converting to a probability over a market's threshold.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Forecast {
    pub lat: f64,
    pub lon: f64,
    pub fetched_at: DateTime<Utc>,
    pub periods: Vec<ForecastPeriod>,
}

/// One 12-hour period in the NWS 7-day forecast. NWS alternates day/night.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForecastPeriod {
    /// "Today", "Tonight", "Wednesday", "Wednesday Night", etc.
    pub name: String,
    /// True for daytime period (corresponds to daily HIGH); false for night (LOW).
    pub is_daytime: bool,
    /// Temperature in °F (NWS always returns Fahrenheit for US points).
    pub temperature_f: i32,
    /// Probability of precipitation in percent (0–100), if NWS provided one.
    pub precipitation_probability_pct: Option<i32>,
    pub start_time: DateTime<Utc>,
    pub end_time: DateTime<Utc>,
    /// Free-form NWS prose ("Sunny, with a high near 76. North wind ...").
    pub detailed_forecast: String,
}

/// Side of a Kalshi binary market.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Side {
    Yes,
    No,
}

/// Output of the strategy: take a position on this market with this size.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Signal {
    pub id: Uuid,
    pub market_ticker: String,
    pub side: Side,
    /// Limit price in dollars (0.0–1.0). Use the resting opposite-side ask.
    pub limit_price: Decimal,
    /// Number of contracts to buy.
    pub contracts: u32,
    /// Model's probability of YES resolving true.
    pub model_yes_probability: Decimal,
    /// Edge: model_yes_probability − market_yes_price (signed for YES side).
    pub edge: Decimal,
    pub generated_at: DateTime<Utc>,
}

/// An open Kalshi position. Kalshi reports positions by market ticker.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Position {
    pub market_ticker: String,
    /// Net YES contracts (positive = long YES, negative = long NO).
    pub net_contracts: i32,
    /// Cost basis in dollars (sum of fills).
    pub cost_basis: Decimal,
    pub opened_at: DateTime<Utc>,
}
