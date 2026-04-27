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

---

## 2026-04-26 (evening) — Backlog item 1: Kalshi market scanner

### What we did
Closed the scanner half of backlog item 1: `KalshiClient` (paginating, rate-limit-aware) + a `WeatherThreshold` parser pinned to real Kalshi tickers. End-to-end run against demo-api.kalshi.co pulls 22 open KXHIGH/KXLOW markets across 10 configured series.

### Scope decision: monitor folded into scanner for v1
The day-0 architecture had `weather-scanner` (discovery) and `weather-monitor` (orderbook polling) as separate crates, mirroring the polymarket bot's split. After looking at Kalshi's actual API: the cheapest way to read prices for N markets is `GET /markets?series_ticker=X` — the same endpoint the scanner already calls — and it returns `yes_bid_dollars` / `yes_ask_dollars` / `last_price_dollars` alongside the metadata. Hitting `/markets/{ticker}` per market is N times more requests for the same data, and Kalshi's rate limits make that a non-starter for >50 markets. So for v1 there's no separate "monitor" — the scanner runs on the `monitor.poll_interval_ms` cadence and doubles as the price feed.

If we ever want sub-poll-interval price latency, the right answer is the Kalshi WebSocket (`/trade-api/ws/v2`) with `orderbook_delta` subscriptions, not REST polling per ticker. That's deferred. The `weather-monitor` crate stays as a placeholder so v2 has somewhere to land.

### Pinning against real Kalshi data first
Before writing any deserializers I curl'd demo-api with `?series_ticker=KXHIGHNY` and inspected the live response. Same discipline as the polymarket bot's "deserializes_real_gamma_field_names" test — synthetic JSON written to match a struct will pass against itself even when the struct is wrong. Two things only the live data revealed:

1. **Prices are JSON strings, not numbers**: `"yes_ask_dollars": "0.4500"`. Deserializer uses `rust_decimal::serde::str_option` to handle this.
2. **`series_ticker` isn't on the market response** — only `event_ticker`. We derive it by stripping the date suffix off `event_ticker` (`KXHIGHNY-26APR27` → `KXHIGHNY`).

### Real ticker grammar (also pinned, not invented)
- `KX{HIGH|LOW}{CITY}-{YYMMDD}-T{N}` + `strike_type=greater` → high/low ≥ N+1°F. Kalshi's "T69 + greater" reads as "high > 69" and integer-rounds to ≥ 70.
- `...-T{N}` + `strike_type=less` → ≤ N-1°F.
- `...-B{n}.5` + `strike_type=between` → bin market (e.g. "high between 64° and 65°"). v1 silently skips these — the simple Normal-CDF model wants one-sided thresholds; bins want a different formulation. Stretch.
- Date format: `YY{JAN|FEB|...|DEC}{DD}` — two-digit year, three-letter uppercase month, two-digit day.

### Rate-limit handling
The first end-to-end smoke run dropped 5 of 10 series fetches to demo-api 429s. The free demo tier rate-limits at roughly 5 RPS; bursting 10 parallel-ish series fetches at startup easily trips it. `KalshiClient::get_with_retry` does exponential backoff (250ms → 500ms → 1s → 2s, total ≤3.75s) on 429 and 503. Re-run picks up all 22 markets cleanly.

### What changed
- `crates/weather-scanner/src/lib.rs` — `KalshiClient`, `MarketScanner`, `RawMarket` deserializer, `parse_weather_threshold`, `parse_kalshi_date`, retry loop. ~250 lines including tests.
- `crates/weather-bot/src/main.rs` — initial scan + periodic refresh task; bot now stays alive on ctrl+C instead of exiting after the "starting" log.
- `config/default.toml` + `crates/weather-config/src/lib.rs` — replaced the placeholder `series_prefixes` with `series_tickers` listing 10 real series (5 cities × 2 stats). Kalshi's `?series_ticker=` filter requires exact match, not a prefix.

### Tests (+9, total now 10)
- 1 fixture-deser test pinning the wire format (catches Kalshi renaming a `_dollars`/`_fp` field).
- 1 `into_market` test for series-ticker derivation + UTC resolution date.
- 5 ticker-parser tests: T-greater, T-less, KXLOW series, between-bin rejection, custom-strike rejection, non-weather-series rejection.
- 1 date-format test (valid + bad month + too-short).
- 1 `#[ignore]`'d live smoke test (`cargo test -p weather-scanner -- --ignored`) that hits demo-api directly. Useful for catching wire-format drift without coupling CI to network.

### State after today
- `cargo test --workspace` clean; 10 tests passing.
- `cargo run -p weather-bot` runs against demo-api, scans 22 markets, idles on a 5s refresh loop until ctrl+C.
- Two commits: `feat(scanner): ...` and `feat(bot): wire scanner into main + retry on Kalshi 429s`.

### What's next (revised)
Original devlog item 1 was scanner + monitor; monitor folded into scanner per the decision above. So the new item 1 is done. The list shifts up:

1. **City code → (lat, lon) mapping.** The threshold parser populates `WeatherThreshold.city` with Kalshi's code ("NY", "CHI", "LAX", ...). The forecast layer needs lat/lon. Small static table; lives in either `weather-types` or a new `weather-locations` module.
2. **Pricing model (`weather-pricing`).** `forecast: Forecast`, `threshold: WeatherThreshold` → `Decimal` model probability. v1: pick the right `ForecastPeriod` (matching `threshold.date` + day/night), apply `Phi((forecast_temp − temp_f) / sigma)` where `sigma` is horizon-indexed.
3. **Strategy + signal generation (`weather-strategy`).**
4. **Risk manager (`weather-risk`).**
5. **Kalshi RSA auth + executor (`weather-executor`).** Demo env first.
6. **Bot main loop wiring everything.** Today's main loop runs the scanner; needs forecast + pricing + strategy + risk + executor stitched in.
7. **Paper-trade for a week.**

