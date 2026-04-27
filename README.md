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
- Workspace compiles, 1 test passes (`weather_forecast::tests::parses_real_nws_forecast_response`).
- NOAA NWS client implemented end-to-end (`NwsClient::fetch_point_forecast`).
- Config + tracing scaffolding lifted from the polymarket bot pattern.

What's stubbed (TODOs in each crate):
- Kalshi REST client (scanner, monitor, executor) — Kalshi auth is RSA-PSS-SHA256, totally different from anything in the polymarket bot.
- Pricing model (Normal-with-fixed-σ for v1).
- Strategy edge filter + Kelly sizing.
- Risk manager.
- Bot main loop wiring the above together.

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
| **scanner** | `series_prefixes` | Only ingest Kalshi markets whose `series_ticker` starts with one of these. v1 default is `["KXHIGH", "KXLOW"]`. |
| **strategy** | `min_edge` | Don't take a position unless `|model_p − market_p| ≥ this`. |
| | `kelly_fraction` | Fraction of full Kelly to bet. 0.25 = quarter-Kelly. |
| **risk** | `max_position_size_usd`, `max_total_exposure_usd`, `per_market_cooldown_secs`, `max_concurrent_positions` | Hard caps. v1 defaults are intentionally tiny. |

---

## Running

```bash
cargo build --release
cargo run --release -p weather-bot
```

Run from the repo root — config is loaded from `config/default.toml` relative to the working directory.

For live trading (when v1 lands): copy `.env.example` → `.env`, set `KALSHI_API_KEY_ID` + `KALSHI_PRIVATE_KEY_PATH`, point the path at the PEM file Kalshi gave you, set `KALSHI_ENV=prod` and `execution.mode = "live"`.

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
