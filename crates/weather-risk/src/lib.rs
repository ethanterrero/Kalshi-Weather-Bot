//! Position sizing, exposure caps, per-market cooldowns.
//!
//! TODO: port the polymarket bot's `RiskManager` shape — it's a clean fit
//! here. Differences: Kalshi positions are integer contracts (not USDC
//! decimals), and resolution can be days out so we hold positions longer.
