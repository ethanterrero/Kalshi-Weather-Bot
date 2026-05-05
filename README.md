# Kalshi Weather Bot

A Rust bot that takes **directional positions** on [Kalshi](https://kalshi.com) weather contracts. The deterministic forecast comes from NOAA NWS; the σ used in pricing comes from a 30-member GEFS ensemble (Open-Meteo). Decisions are gated by an edge filter, a price band, an NWS-update lockout, and per-market / per-pass risk caps, and are persisted as JSONL for backtesting.

For the live backlog see **[ROADMAP.md](ROADMAP.md)**; for the build narrative see **[devlog.md](devlog.md)**.

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
- **Settlement-source validation**: every Kalshi city code (`NY`, `CHI`, `LAX`, `MIA`, `AUS`) is mapped to the ICAO station whose **NWS Daily Climate Report (CLI)** Kalshi settles on (`KNYC`, `KMDW`, `KLAX`, `KMIA`, `KAUS`). The pricing layer refuses to model an unmapped city — better to abstain than to compare a forecast to the wrong station.
- **Standard-time settlement window**: per the [Kalshi help page](https://help.kalshi.com/en/articles/13823837-weather-markets), high-temperature settlement uses **local standard time** even during DST. `weather-types::tempwindow` builds the half-open UTC window `[date 00:00 LST, date+1 00:00 LST)` and the pricing layer matches forecast periods against it, so a summer NYC market is evaluated against UTC `[05:00, 05:00 next day)`, never `[04:00, 04:00 next day)`.
- **Probabilistic, not deterministic**: instead of "forecast > threshold → buy YES", we compute `P(YES) = 1 - Φ((T - μ) / σ)`. At μ ≈ T this gives ~50/50 — exactly when the market should be a coin flip too. Edge appears in the tails.
- **σ source**: in production we use a 30-member GFS ensemble (Open-Meteo) and take the population standard deviation of per-member daily highs/lows inside the same Kalshi standard-time window pricing already uses. When the ensemble fetch fails or no in-window members are available, we fall back to a hand-calibrated horizon-indexed table (1.6°F at day 0 → 5°F at day 7). Every JSONL decision row stamps `sigma_source` so the backtester can split metrics later. ECMWF blending is the next σ source; AWS GEFS reanalysis is the rigorous historical backtest path.

---

## Architecture

Rust workspace with **eleven crates**. The shape mirrors the polymarket-arbitrage-bot but the contents are different — directional ≠ arb.

```
┌──────────────┐   ┌──────────────┐   ┌────────────────┐   ┌─────────────────────┐
│ MarketScanner│   │ NwsClient    │   │ PricingModel   │   │ Strategy            │
│ (Kalshi /    │   │ + GefsClient │──▶│ Normal-CDF +   │──▶│ edge gate, price    │
│  markets)    │   │  σ ensemble  │   │ σ override     │   │ band, fresh lockout │
└──────┬───────┘   └──────────────┘   └────────────────┘   └──────────┬──────────┘
       │                                                              │
       ▼                                                              ▼
┌──────────────┐                                            ┌─────────────────────┐
│ DecisionLogger│  ◀───  every market every pass  ◀──────  │ RiskManager         │
│ JSONL/day    │                                            │ position cap +      │
│              │                                            │ exposure cap +      │
└──────────────┘                                            │ concurrent cap      │
                                                            └──────────┬──────────┘
                                                                       │
                                                                       ▼
┌────────────────┐    ┌─────────────────┐    ┌─────────────────────────────────┐
│ IemCliClient   │    │ HistoricalForecast│  │ KalshiOrderClient               │
│ (settlement    │    │ (calibrate binary│   │ RSA-PSS-SHA256 signed           │
│  ground truth) │    │  archive)        │   │ POST /portfolio/orders          │
└────────────────┘    └─────────────────┘    │ ⚠ never_send=true (kill-switch) │
       │                      │              └─────────────────────────────────┘
       ▼                      ▼
   weather-backtest:  metrics(joined)  →  hit rate / Brier / log-loss / calibration
```

| Component | Crate | Role |
|---|---|---|
| **Market Scanner** | `weather-scanner` | Polls Kalshi `GET /markets?series_ticker=…`, parses ticker → `WeatherThreshold`. 429-aware retries. |
| **Monitor** *(stub)* | `weather-monitor` | Reserved for the eventual WebSocket orderbook-delta feed. Today the scanner doubles as the price feed. |
| **Forecast** | `weather-forecast` | Four sources: `NwsClient` (deterministic 7-day), `GefsClient` (Open-Meteo 30-member ensemble σ), `IemCliClient` (CLI ground truth), `HistoricalForecastClient` (Open-Meteo archive for calibration). |
| **Pricing** | `weather-pricing` | Forecast + threshold → model probability via Normal-CDF + continuity correction. `price_market_with_sigma(_, _, sigma_override)` accepts a runtime σ; output stamps `sigma_source`. |
| **Strategy** | `weather-strategy` | `Signal` generation: edge gate, price band `[$0.20, $0.92]`, NWS-update lockout, fee-aware EV, Kelly sizing. |
| **Risk** | `weather-risk` | Per-market position cap, per-pass total-exposure cap, concurrent-positions cap. |
| **Executor** | `weather-executor` | Kalshi RSA-PSS-SHA256 signer + `POST /portfolio/orders` request shape. Hard-coded `never_send=true` short-circuits before HTTP — disabling is an explicit code call. |
| **Backtest** | `weather-backtest` | JSONL replay + IEM CLI join + headline metrics (hit rate, Brier, log-loss, calibration). Includes the `calibrate` binary for pre-dry-run model calibration. |
| **Bot** | `weather-bot` | Entrypoint + main loop. Owns the JSONL `DecisionLogger` and the NWS / GEFS caches. |
| **Config** | `weather-config` | Loads `config/default.toml` + env overrides (`__` separator). |
| **Types** | `weather-types` | Shared domain types + DST-aware standard-time settlement-window helpers. |

---

## Status

End-to-end runnable in dry-run. **116 unit tests + 3 ignored** live-network integration tests; CI enforces `fmt/clippy -D warnings/build/test --locked` on every PR.

What's real:
- **Pricing**: Normal-CDF model with continuity correction + horizon-indexed σ as the *fallback*. Live σ comes from a 30-member GFS ensemble (Open-Meteo) when in-window members are available; output stamps `sigma_source: "gefs_ensemble" | "static"` so the JSONL can split metrics later.
- **Sources**: NOAA NWS deterministic 7-day, Open-Meteo GEFS ensemble, Iowa State IEM CLI (settlement ground truth), Open-Meteo historical-forecast archive (calibration only).
- **Strategy gates**: fee-aware EV, both YES and NO sides, price band `[$0.20, $0.92]` (excludes blow-up tail contracts), 30-min post-NWS-update lockout, settlement-station validation against the 5-city table.
- **Risk**: per-market position cap, per-pass total-exposure cap, concurrent-positions cap. Wired into the strategy loop.
- **Decision log**: `logs/decisions/YYYY-MM-DD.jsonl`, one row per market per pass, both Trade and NoTrade. Schema includes `sigma_source` for split-by-source backtests.
- **Calibration runner**: `cargo run -p weather-backtest --bin calibrate -- --city NY --start 2025-04-01 --end 2025-09-30 --high-strikes 60,65,70,75,80,85,90,95` — pulls archived GFS + IEM CLI, prints headline metrics + 10-bucket calibration histogram.
- **Executor**: Kalshi RSA-PSS-SHA256 auth, `OrderRequest::from_signal`, `POST /portfolio/orders`. **Kill-switched** via a hard-coded `never_send=true` flag — disabling requires an explicit `client.allow_real_sends()` code edit, not a config knob.

What's still pending:
- The executor isn't yet called from the strategy loop (auth path exists but the bot doesn't hand `Decision::Trade` over to it).
- Per-market cooldown, per-(city, date) correlated-exposure cap, bankroll-fraction Kelly.
- WebSocket orderbook deltas (v1 uses REST polling).
- Historical Kalshi prices for "net-of-fee expectancy >0 per trade" backtest criterion.
- ECMWF blending, METAR/intraday observations.

See **[ROADMAP.md](ROADMAP.md)** for the live backlog and **[devlog.md](devlog.md)** for the build narrative.

---

## Configuration

Edit `config/default.toml`. Override with env vars using `__` (e.g. `STRATEGY__MIN_EDGE=0.07`).

| Section | Key | Meaning |
|---|---|---|
| **kalshi** | `env` | `"demo"` (paper trading at demo-api.kalshi.co) or `"prod"` (real money at trading-api.kalshi.com). |
| **execution** | `mode` | `"dry_run"` (default) or `"live"`. Live additionally requires `KALSHI_API_KEY_ID` + `KALSHI_PRIVATE_KEY_PATH` env vars and `kalshi.env="prod"`. Note: the executor's `never_send` kill-switch is *separate* and hard-coded in code; flipping `mode` to `"live"` is not enough to actually place orders. |
| **forecast** | `nws_base_url` | NOAA NWS base. Default `https://api.weather.gov`. |
| | `user_agent` | NWS rejects requests without a meaningful UA. **Edit this with your contact email.** |
| | `refresh_interval_secs` | How often to re-poll NWS forecasts. Default 1800. |
| | `nws_lockout_after_update_secs` | Sit out trades for this many seconds after each NWS forecast issue (`generatedAt`). Default 1800; 0 to disable. |
| | `gefs_sigma_enabled` | Use Open-Meteo's 30-member GEFS ensemble σ in pricing instead of the hand-calibrated `sigma_for_horizon` table. Static table stays as the fallback. Default `true`. |
| | `gefs_refresh_interval_secs` | GEFS poll cadence. GEFS publishes new runs every ~6h; 1800 (30 min) is fine. |
| **scanner** | `series_tickers` | Full Kalshi series tickers to ingest (e.g. `KXHIGHNY`). Kalshi's `?series_ticker=` filter requires exact match, not a prefix. |
| | `refresh_interval_secs` | Kalshi `/markets` poll cadence. Default 300. |
| **strategy** | `min_edge` | Don't take a position unless `|model_p − market_p| ≥ this`. Default 0.05. |
| | `kelly_fraction` | Fraction of full Kelly to bet. 0.25 = quarter-Kelly. |
| | `safety_buffer` | Extra dollars-per-contract margin the EV gate requires on top of fees. Default 0.01. |
| | `fee_multiplier` | Per-series Kalshi fee multiplier (1.0 for most weather markets). |
| | `max_spread` | Maximum bid-ask spread the bot will trade across. Default 0.10. |
| | `min_price` / `max_price` | Price band — tail-bracket contracts (cheap or near-1.00) carry asymmetric blow-up risk and are excluded. Defaults 0.20 / 0.92. |
| **risk** | `max_position_size_usd`, `max_total_exposure_usd`, `per_market_cooldown_secs`, `max_concurrent_positions` | Hard caps. v1 defaults are intentionally tiny ($5/market, $50 total, 10 concurrent). |
| **monitor** | `poll_interval_ms` | Strategy-loop tick. Default 5000. |
| **logging** | `level` | `tracing` filter, e.g. `"info"`. |
| | `json_output` | Emit structured JSON logs to stderr instead of human-readable. Default `false`. |
| | `decision_log_dir` | Directory for the per-day JSONL decision log. Default `"logs/decisions"`; `null`/empty disables. |

---

## Running

### The bot

```bash
cargo build --release
cargo run --release -p weather-bot
```

Run from the repo root — config is loaded from `config/default.toml` relative to the working directory.

The bot is **safe by default**: `execution.mode = "dry_run"` and the executor's `never_send=true` kill-switch are *both* on. The bot scans markets, runs the model, evaluates risk caps, and writes JSONL decisions, but **places no orders**. Flipping `mode = "live"` is not enough — disabling `never_send` requires a code edit (`client.allow_real_sends()` in [crates/weather-executor/src/orders.rs](crates/weather-executor/src/orders.rs)).

For (eventual) live trading: copy `.env.example` → `.env`, set `KALSHI_API_KEY_ID` + `KALSHI_PRIVATE_KEY_PATH`, point the path at the PEM file Kalshi gave you, set `KALSHI_ENV=prod` and `execution.mode = "live"`. The bot doesn't yet wire the executor into the strategy loop — that's an outstanding ROADMAP item.

### The calibration runner

Pre-flight check on the model. Note: 14 days of live dry-run is realistically only ~20–80 trade-eligible decisions after the gates fire, which isn't enough to fill the calibration histogram with statistical power. The dry-run is a **smoke test** (does the bot run, do gates fire, are sources healthy?). The actual **calibration evidence** comes from running the synthetic-decision backtest harness over months of historical data:

```bash
cargo run -p weather-backtest --bin calibrate -- \
    --city NY --start 2025-04-01 --end 2025-09-30 \
    --high-strikes 60,65,70,75,80,85,90,95
```

Pulls archived deterministic GFS forecasts (Open-Meteo) and IEM CLI ground truth, walks the strike grid through the same Normal-CDF + continuity-correction the live bot uses, and prints hit rate / Brier / log-loss / mean bias / 10-bucket calibration. `--sigma N.N` overrides the default day-0 σ (1.6°F).

### Reading the dry-run logs

A successful trade decision logs at INFO level with the full EV math:

```
TRADE (dry-run) ticker=KXHIGHNY-26JUL04-T75 side=Yes limit_price=0.50
  contracts=37 model_p=0.85 market_p=0.50 spread=0.05
  fee_est=0.02 raw_edge=0.35 net_ev=0.33
  station=KNYC forecast_temp_f=78 horizon_days=1 sigma_f=2.4
```

A no-trade decision logs the explicit reason — at DEBUG for the common cases (`EdgeBelowMin`, `EvBelowGate`) so the INFO stream isn't flooded, and at INFO for the louder ones (`SpreadTooWide`, `NoOrderbook`, `PriceOutOfBand`, `ForecastTooFresh`). Set `RUST_LOG=weather_bot=debug` to see every market's decision.

The same decision (Trade or NoTrade) is also written as JSON to `logs/decisions/YYYY-MM-DD.jsonl` — one row per market per pass, regardless of log level. That's the substrate the backtester reads.

## Limitations (read this before trusting the bot)

- **σ is now ensemble-derived in production**, but the hand-calibrated `sigma_for_horizon` table is still the fallback whenever an ensemble fetch fails or no in-window members are available. Whether real ensemble σ improves calibration vs the static table is an *empirical* question — answer comes from running the bot in dry-run for ~2 weeks and splitting `metrics()` by `sigma_source`.
- **σ is per-day, not per-city or per-season.** A summer NYC σ and a winter LAX σ get the same value. Phase 4 calls for per-(city, season) σ once we have the data.
- **Single-city ensemble fetch per market.** No ECMWF blending yet. Open-Meteo supports `models=ecmwf_ifs025` on the same endpoint; that's a small follow-up PR.
- **No METAR / intraday observation feed.** Same-day "lock" trades (per Perplexity's research, the highest retail edge) need the running max from the airport sensor — not yet wired.
- **No WebSocket**. Prices come from REST polling on `monitor.poll_interval_ms` (default 5s). For thin weather markets this is fine; a stale-price gate using `orderbook_delta` ([Kalshi WS docs](https://docs.kalshi.com/getting_started/quick_start_websockets)) is deferred.
- **Settlement validation is by static table.** Five city codes are mapped to ICAO stations by hand. If Kalshi adds a city we don't know, the bot abstains rather than guesses.
- **No live order placement (yet).** The executor's auth path is correct against the Kalshi spec, but the strategy loop doesn't currently call it. Wiring up requires both `mode = "live"` AND a code edit to disable `never_send`.

---

## Project Layout

```
├── .github/workflows/
│   └── ci.yml              # build/test/fmt/clippy gates
├── config/
│   └── default.toml
├── crates/
│   ├── weather-types/      # shared types + tempwindow + city table
│   ├── weather-config/     # config loader + env override
│   ├── weather-scanner/    # Kalshi /markets
│   ├── weather-monitor/    # (placeholder for WS orderbook deltas)
│   ├── weather-forecast/   # NWS + GEFS + IEM CLI + historical GFS
│   ├── weather-pricing/    # forecast → P(threshold), σ override
│   ├── weather-strategy/   # edge gate, price band, lockout, Kelly
│   ├── weather-risk/       # position + exposure + concurrent caps
│   ├── weather-executor/   # Kalshi RSA auth + order placement (kill-switched)
│   ├── weather-backtest/   # JSONL replay + metrics + calibrate binary
│   └── weather-bot/        # entrypoint
├── logs/decisions/         # per-day JSONL decision log (gitignored)
├── ROADMAP.md
├── devlog.md
├── .env.example
└── README.md
```
