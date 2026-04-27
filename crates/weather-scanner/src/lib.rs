//! Kalshi market scanner.
//!
//! TODO: implement against `GET /markets` on the Kalshi v2 API. Filter by
//! `series_prefixes` from config and parse the city/threshold/date out of
//! each ticker into a `WeatherThreshold`. v1 will only handle daily highs
//! and lows.
