//! Kalshi orderbook + market-state monitor.
//!
//! TODO: poll `GET /markets/{ticker}` (or the WebSocket if it's worth the
//! complexity for thin markets) for each tracked market and push fresh
//! `KalshiMarket` snapshots into a channel for the strategy to consume.
