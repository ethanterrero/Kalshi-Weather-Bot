# Kalshi Weather Bot

A Rust bot that takes **directional positions** on [Kalshi](https://kalshi.com) weather contracts using NOAA NWS forecasts as the model price.

For day-to-day engineering notes and the ordered backlog, see **[devlog.md](devlog.md)**.

---

## How It Works

### The Idea

Kalshi lists binary contracts on weather outcomes — e.g. *"Will the high temperature in NYC on 2026-07-04 be ≥ 75°F?"* — that pay $1 if YES resolves true and $0 if it doesn't. The market price of the YES contract is, by no-arb, the market's collective probability estimate for the event.

If your **forecast-derived probability** disagrees with the market by more than a configurable margin, that's an edge. The bot:

1. Pulls the NOAA NWS 7-day point forecast for the city the contract references.
2. Converts the point forecast into a probability over the contract's threshold (next-day temperature errors are ~Normal with σ ≈ 2°F; the dispersion grows with horizon).
3. Compares to the Kalshi market price, sizes by Kelly fraction, and submits a limit order at the best resting opposite-side price.

This is **not arbitrage** — it's a forecast-quality bet. The bot loses if NOAA's forecast is wrong more often than the Kalshi market thinks.

### The edge framework (fee + spread + buffer gate)

A raw "model probability beats market price by 5¢" condition isn't enough — round-trip costs eat thin edges. Every market is run through this gate before the bot will act:

```
net_ev_per_contract = our_prob - price - estimated_fee
trade if   raw_edge ≥ min_edge
       AND spread ≤ max_spread
       AND net_ev_per_contract ≥ safety_buffer
```

- **Estimated fee** uses Kalshi's published formula `multiplier * 0.07 * p * (1 - p)` per contract, rounded up to the next cent. Implemented in `weather-strategy::fees`.
- **Both YES and NO sides** are evaluated. The implied NO ask is `1 - yes_bid` (Kalshi binary no-arb). The bot picks the side with the larger raw edge.
- **Settlement-source validation**: every Kalshi city code (`NY`, `CHI`, `LAX`, `MIA`, `AUS`) is mapped to the ICAO station whose **NWS Daily Climate Report (CLI)** Kalshi settles on (`KNYC`, `KORD`, `KLAX`, `KMIA`, `KAUS`). The pricing layer refuses to model an unmapped city — better to abstain than to compare a forecast to the wrong station.
- **Standard-time settlement window**: per the [Kalshi help page](https://help.kalshi.com/en/articles/13823837-weather-markets), high-temperature settlement uses **local standard time** even during DST. `weather-types::tempwindow` builds the half-open UTC window `[date 00:00 LST, date+1 00:00 LST)` and the pricing layer matches forecast periods against it, so a summer NYC market is evaluated against UTC `[05:00, 05:00 next day)`, never `[04:00, 04:00 next day)`.
- **Probabilistic, not deterministic**: instead of "forecast > threshold → buy YES", we compute `P(YES) = 1 - Φ((T - μ) / σ)` with a horizon-indexed σ (1.6°F at day 0 → 5°F at day 7). At μ ≈ T this gives ~50/50 — exactly when the market should be a coin flip too. Edge appears in the tails. This is the slot where an ensemble source (NOAA GEFS, ECMWF open data, Open-Meteo ECMWF) can drop in later to replace fixed σ with realized ensemble dispersion.

---

## Architecture

Rust workspace with ten crates. The shape mirrors the polymarket-arbitrage-bot but the contents are different — directional ≠ arb.

```
┌──────────────┐    ┌──────────────┐    ┌────────────────┐    ┌────────────┐
│ MarketScanner│    │ NwsClient    │    │ PricingModel   │    │ Strategy   │
│ (Kalshi /    │    │ (NOAA NWS    │    │ forecast →     │───▶│ edge ≥ min │
│  markets)    │    │  forecast)   │    │ P(threshold)   │    │ → Signal   │
└──────┬───────┘    └──────┬───────┘    └────────┬───────┘    └─────┬──────┘
       │                   │                     │                   │
       ▼                   └─────────────────────┘                   ▼
┌──────────────┐                                              ┌────────────┐
│ Monitor      │                                              │ RiskMgr    │
│ (orderbook   │─────────────────────────────────────────────▶│ Kelly +    │
│  poll/WS)    │                                              │ caps       │
└──────────────┘                                              └─────┬──────┘
                                                                    │
                                                                    ▼
                                                              ┌────────────┐
                                                              │ Executor   │
                                                              │ (Kalshi    │
                                                              │  RSA auth) │
                                                              └────────────┘
```

| Component | Crate | Role |
|---|---|---|
| **Market Scanner** | `weather-scanner` | Polls Kalshi `GET /markets`, filters by `series_prefixes`, parses ticker → `WeatherThreshold`. |
| **Monitor** | `weather-monitor` | Refreshes orderbook + last-trade snapshots for tracked markets. |
| **Forecast** | `weather-forecast` | NOAA NWS 7-day point forecast client. Two-step flow (`/points/{lat,lon}` → `/gridpoints/.../forecast`). |
| **Pricing** | `weather-pricing` | Forecast + threshold → model probability. |
| **Strategy** | `weather-strategy` | `Signal` generation: edge filter + Kelly sizing. |
| **Risk** | `weather-risk` | Position size caps, total exposure cap, per-market cooldown. |
| **Executor** | `weather-executor` | Kalshi RSA-PSS-SHA256 signed `POST /portfolio/orders`. |
| **Bot** | `weather-bot` | Entrypoint + main loop. |
| **Config** | `weather-config` | Loads `config/default.toml` + env overrides (`__` separator). |
| **Types** | `weather-types` | Shared domain types. |

---

## Status (v0)

What's real today:
- Workspace compiles cleanly; 36 unit tests passing across types/pricing/strategy/scanner/forecast.
- NOAA NWS client implemented end-to-end (`NwsClient::fetch_point_forecast`).
- Kalshi `/markets` scanner (paginating, 429-aware) + ticker-parser pinned to live demo-api responses.
- Probabilistic pricing model (Normal-CDF with continuity correction, horizon-indexed σ) with settlement-station validation.
- Fee-aware EV gate evaluating both YES and NO sides; explicit `NoTrade` reason logged when nothing fires.
- DST / local-standard-time settlement window helper.
- Bot main loop runs the full forecast → pricing → strategy pipeline in dry-run.

What's stubbed (TODOs in each crate):
- Kalshi REST executor (RSA-PSS-SHA256 auth + `POST /portfolio/orders`).
- Risk manager (position caps, cooldowns, total-exposure check).
- WebSocket-based orderbook deltas (v1 uses REST polling on `monitor.poll_interval_ms`).

See **[devlog.md](devlog.md)** for the ordered backlog.

---

## Configuration

Edit `config/default.toml`. Override with env vars using `__` (e.g. `STRATEGY__MIN_EDGE=0.07`).

| Section | Key | Meaning |
|---|---|---|
| **kalshi** | `env` | `"demo"` (paper trading at demo-api.kalshi.co) or `"prod"` (real money at trading-api.kalshi.com). |
| **execution** | `mode` | `"dry_run"` (default) or `"live"`. Live additionally requires `KALSHI_API_KEY_ID` + `KALSHI_PRIVATE_KEY_PATH` env vars and `kalshi.env="prod"`. |
| **forecast** | `nws_base_url` | NOAA NWS base. Default `https://api.weather.gov`. |
| | `user_agent` | NWS rejects requests without a meaningful UA. **Edit this with your contact email.** |
| | `refresh_interval_secs` | How often to re-poll forecasts. |
| **scanner** | `series_tickers` | Full Kalshi series tickers to ingest (e.g. `KXHIGHNY`). Kalshi's `?series_ticker=` filter requires exact match, not a prefix. |
| **strategy** | `min_edge` | Don't take a position unless `|model_p − market_p| ≥ this`. |
| | `kelly_fraction` | Fraction of full Kelly to bet. 0.25 = quarter-Kelly. |
| | `safety_buffer` | Extra dollars-per-contract margin the EV gate requires on top of fees. |
| | `fee_multiplier` | Per-series Kalshi fee multiplier (1.0 for most weather markets). |
| | `max_spread` | Maximum bid-ask spread the bot will trade across. |
| **risk** | `max_position_size_usd`, `max_total_exposure_usd`, `per_market_cooldown_secs`, `max_concurrent_positions` | Hard caps. v1 defaults are intentionally tiny. |

---

## Running

```bash
cargo build --release
cargo run --release -p weather-bot
```

Run from the repo root — config is loaded from `config/default.toml` relative to the working directory.

The bot is **safe by default**: `execution.mode = "dry_run"` means it scans markets, runs the model, and logs trade decisions, but **places no orders**. Even if you set `mode = "live"` today, the executor crate is still a stub — the bot will warn and stay in dry-run.

For (eventual) live trading: copy `.env.example` → `.env`, set `KALSHI_API_KEY_ID` + `KALSHI_PRIVATE_KEY_PATH`, point the path at the PEM file Kalshi gave you, set `KALSHI_ENV=prod` and `execution.mode = "live"`.

### Reading the dry-run logs

A successful trade decision logs at INFO level with the full EV math:

```
TRADE (dry-run) ticker=KXHIGHNY-26JUL04-T75 side=Yes limit_price=0.50
  contracts=37 model_p=0.85 market_p=0.50 spread=0.05
  fee_est=0.02 raw_edge=0.35 net_ev=0.33
  station=KNYC forecast_temp_f=78 horizon_days=1 sigma_f=2
```

A no-trade decision logs the explicit reason — at DEBUG for the common cases (`EdgeBelowMin`, `EvBelowGate`) so the INFO stream isn't flooded, and at INFO for the louder ones (`SpreadTooWide`, `NoOrderbook`). Set `RUST_LOG=weather_bot=debug` to see every market's decision.

## Limitations (read this before trusting the bot)

- **σ is hand-calibrated**, not learned. Realized NWS errors vary by city, season, and synoptic regime. Plug an ensemble source (NOAA GEFS, ECMWF open data, Open-Meteo) into `weather-pricing::sigma_for_horizon` to do better.
- **No risk manager wired in yet**. The strategy emits a contract-count hint; an actual risk layer with position caps and total-exposure checks needs to gate signals before any executor runs.
- **No WebSocket**. Prices come from REST polling on `monitor.poll_interval_ms` (default 5 s). For thin weather markets that's fine, but a stale-price gate using `orderbook_delta` ([Kalshi WS docs](https://docs.kalshi.com/getting_started/quick_start_websockets)) is the obvious next improvement.
- **Settlement validation is by static table**. We map five city codes to ICAO stations by hand. If Kalshi adds a city we don't know, the bot abstains rather than guesses.
- **NWS forecast is the only model input**. No ensemble blending, no METAR observation feed, no preliminary CLI gating against the [delayed-settlement risks](https://help.kalshi.com/en/articles/13823837-weather-markets) noted in Kalshi's help page.

---

## Project Layout

```
├── config/
│   └── default.toml
├── crates/
│   ├── weather-types/
│   ├── weather-config/
│   ├── weather-scanner/    # Kalshi /markets
│   ├── weather-monitor/    # Kalshi orderbook polling
│   ├── weather-forecast/   # NOAA NWS client
│   ├── weather-pricing/    # forecast → P(threshold)
│   ├── weather-strategy/   # edge + Kelly → Signal
│   ├── weather-risk/       # caps + cooldowns
│   ├── weather-executor/   # Kalshi auth + order placement
│   └── weather-bot/        # entrypoint
├── devlog.md
├── .env.example
└── README.md
```
