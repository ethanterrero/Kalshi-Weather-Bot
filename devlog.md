# Dev Log

---

## 2026-04-26 — Day 0: scaffold + NOAA NWS client

### Decision: directional, not arbitrage
Forked the polymarket-arbitrage-bot mental model but not the code. The polymarket bot's whole reason for existing — buy YES + buy NO when the pair costs < $1 — doesn't apply to Kalshi weather. There's no synthetic risk-free leg; the bet is purely on whether NOAA's forecast is more accurate than the Kalshi market price implies.

That changes the architecture meaningfully:
- No `arb-strategy` (arbitrage detector) → replaced by `weather-pricing` (forecast → fair-value probability) + `weather-strategy` (edge + Kelly sizing).
- No `arb-inventory` (asymmetric/hybrid leg pairing) → not applicable; positions are independent bets, no pairing needed.
- No EIP-712, no Polygon RPC, no allowance check — Kalshi has no on-chain component. Auth is RSA-PSS-SHA256 + API key in headers.
- New: `weather-forecast` (NOAA NWS client). The polymarket bot had no equivalent — Polymarket's "fair value" is just the orderbook.

### Decisions: v1 scope
- **Forecast source:** NOAA NWS only. Free, no auth, decent for US markets (which is what Kalshi weather covers). Open-Meteo as a backup multi-model source is on the wishlist but not v1.
- **Markets in scope:** daily temperature highs and lows, top US cities (NYC, CHI, LAX, MIA, ...). Most liquid Kalshi weather contracts. Rainfall and snowfall are messier to model and have wider spreads — stretch goals.
- **Pricing model:** Normal forecast errors with fixed σ per horizon (next-day ~2°F, +5d ~5°F based on published NWS verification stats). Calibrate σ later from observed errors.
- **Edge filter:** `|model_p − market_p| ≥ 5¢` to enter. Quarter-Kelly sizing.
- **Risk:** $5 max per position, $50 total exposure, 10 concurrent positions max. v1 is intentionally tiny — directional weather edges are noisy and Kalshi weather liquidity is thin.

### What landed today
- Rust workspace with ten crates: `weather-types`, `weather-config`, `weather-scanner`, `weather-monitor`, `weather-forecast`, `weather-pricing`, `weather-strategy`, `weather-risk`, `weather-executor`, `weather-bot`. Eight are skeletons with TODO comments; two are real:
  - **`weather-types`** — `KalshiMarket`, `WeatherThreshold`, `Forecast`, `ForecastPeriod`, `Signal`, `Position`, `Side`, `TempStat`, `ThresholdDirection`. Naming follows Kalshi's actual API vocabulary (series → event → market).
  - **`weather-forecast`** — `NwsClient::fetch_point_forecast(lat, lon)` implements the standard NWS two-step flow (`/points/{lat,lon}` to discover the gridpoints URL, then GET that URL for the 7-day forecast). Deserializer pulls camelCase fields from real NWS responses; nullable `probabilityOfPrecipitation.value` round-trips to `Option<i32>`. One unit test against a captured NYC fixture asserts the field mapping.
- Config (`config/default.toml`), `.env.example`, `.gitignore`. All env vars are off by default — bot runs in dry-run with no credentials.
- Logging via `tracing` + `tracing-subscriber`, dotenv loading, all the same scaffolding patterns from the polymarket bot.
- `cargo build --workspace` clean. `cargo test --workspace` green (1 test passing).

### What's next (in order)
1. **Kalshi REST client (`weather-scanner` + `weather-monitor`)** — `GET /markets` with the `series_prefixes` filter, ticker-parser to populate `WeatherThreshold` from a `KalshiMarket`, then orderbook polling for tracked markets.
2. **Pricing model (`weather-pricing`)** — Normal CDF over `(forecast_temp − threshold) / σ`, with a horizon-indexed σ table.
3. **City → (lat, lon) mapping** — Kalshi tickers reference cities by code (NY, CHI, ...). Need a small static table mapping each supported city to the NWS forecast point. Probably lives in `weather-types` or a dedicated `weather-locations` module.
4. **Strategy (`weather-strategy`)** — given a `KalshiMarket` + a fresh `Forecast`, produce a `Signal` if edge clears `min_edge`. Quarter-Kelly contracts.
5. **Risk (`weather-risk`)** — port the polymarket bot's `RiskManager` shape; integer-contract sizing instead of USDC decimals.
6. **Executor (`weather-executor`)** — Kalshi auth (load PEM, RSA-PSS-SHA256 over `timestamp + method + path`, three custom headers), then `POST /portfolio/orders`. Test against the demo environment first.
7. **Bot main loop (`weather-bot`)** — wire everything together. Polling loop on the forecast (every 30 min) and the markets (every few seconds). Log opportunity → risk → execute.
8. **First demo-env paper-trading session.** Watch the bot for a week before considering `KALSHI_ENV=prod`.

### Concept learned: directional bots need a forecast layer arb bots don't
The polymarket bot has no "what's fair value" component because arb's fair value is mechanical: `1.0 - opposite_ask`. Directional trading collapses if you can't generate a model probability, so `weather-forecast` + `weather-pricing` are now load-bearing crates rather than nice-to-haves. That's also why dry-run alone won't validate the strategy — the bot can run cleanly in dry-run while still having a useless model. We'll need backtest tooling (replay historical NWS forecasts vs Kalshi closing prices) before live trading is meaningful.

### State after today
- 10 crates, ~600 LoC of Rust (most of it types + the NWS client).
- 1 test passing.
- Branch: `main` (no PR — initial commit).

