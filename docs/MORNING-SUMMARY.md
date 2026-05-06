# Morning summary — 2026-05-05 overnight session

> tl;dr: Every safety prerequisite for live trading is now in place. The
> bot has a real dynamic kill switch, paper-mode plumbing, per-market
> cooldown, proper bankroll-fraction Kelly, source-health checks, mode
> tagging on every log line, a `--once` CLI flag, doc-tests on the
> pricing math, and four research artifacts on the next round of work
> (Kalshi historical candles, METAR/ASOS, paper-trade design, ECMWF
> blending). All workspace tests green; fmt + clippy clean.

## What landed (code)

### Safety stack (the headline)

| Item | Where | Tests |
|---|---|---|
| Dynamic kill switch (file flag + env var + drawdown soft-kill) | [crates/weather-bot/src/kill_switch.rs](crates/weather-bot/src/kill_switch.rs) | 9 unit |
| SIGTERM handler in main loop | [crates/weather-bot/src/main.rs](crates/weather-bot/src/main.rs) | covered by manual run |
| `Paper` execution mode + `mode` tag on JSONL | [crates/weather-config/src/lib.rs](crates/weather-config/src/lib.rs), [crates/weather-bot/src/decision_log.rs](crates/weather-bot/src/decision_log.rs) | 4 unit |
| Per-market cooldown in `RiskManager` | [crates/weather-risk/src/lib.rs](crates/weather-risk/src/lib.rs) | 6 unit |
| Bankroll-fraction Kelly | [crates/weather-strategy/src/lib.rs](crates/weather-strategy/src/lib.rs) | 4 unit |

### Operability

| Item | Where |
|---|---|
| `--once` CLI flag | [crates/weather-bot/src/main.rs:35](crates/weather-bot/src/main.rs:35) |
| Source health checks (NWS + GEFS staleness) | [crates/weather-bot/src/source_health.rs](crates/weather-bot/src/source_health.rs) |
| Risk reject reasons in per-pass summary | [crates/weather-bot/src/main.rs](crates/weather-bot/src/main.rs) |
| Per-pass summary now has `risk_in_cooldown`, `risk_concurrent_capped`, `risk_no_budget`, `nws_source_stale`, `gefs_source_stale` | same |

### Quality / cleanup

| Item | Where |
|---|---|
| `OrderbookQuote` unified into `weather-types` | [crates/weather-types/src/lib.rs](crates/weather-types/src/lib.rs) |
| Pricing doc-tests (μ=T → P≈0.5, Φ(0)=0.5, σ monotone) | [crates/weather-pricing/src/lib.rs](crates/weather-pricing/src/lib.rs) |
| `f64`-money audit | [docs/research/f64-money-audit.md](docs/research/f64-money-audit.md) |

## What landed (research, in `docs/research/`)

| File | Headline |
|---|---|
| [kalshi-historical-candles.md](docs/research/kalshi-historical-candles.md) | `GET /trade-api/v2/historical/markets/{ticker}/candlesticks` exists. Same shape as live route; `/historical/cutoff` decides which to call. Recommended: chunked queries, no archive scraping needed. |
| [metar-observations.md](docs/research/metar-observations.md) | All 5 ICAOs publish METAR; CLI station == observation station for every city. `api.weather.gov/stations/{ICAO}/observations[/latest]`. Units are SI. `maxTemperatureLast24Hours` is null in practice — derive running daily max from hourly observations ourselves. IEM `currents.json` is the recommended fallback. |
| [paper-trade-mode.md](docs/research/paper-trade-mode.md) | **Demo environment is fully writable** — accepts the full order lifecycle (`CreateOrder`/`CancelOrder`/`AmendOrder`/etc.), with its own WS for `orderbook_delta`/`fill`/`market_positions`. Credentials separate from prod (mint an RSA key pair at demo.kalshi.co). Demo accounts are not pre-funded. Recommendation: ship demo-first, build the local mock only if demo's matching engine doesn't exercise the lifecycle bugs we care about. |
| [ecmwf-open-meteo.md](docs/research/ecmwf-open-meteo.md) | `models=ecmwf_ifs025`, **51 members** (50 + 1 control). Response shape identical to GEFS — existing parser handles it unchanged. Recommended: add `fetch_ecmwf_ifs025` next to `fetch_gfs05` on the same client. Blender option A: pool members into one combined sample; option B: defensive max-σ. |
| [f64-money-audit.md](docs/research/f64-money-audit.md) | Money paths are end-to-end `Decimal`. The single `f64 → Decimal` boundary is `Decimal::from_f64(yes_p)` in pricing. No action required. |

## How to verify

```bash
cargo build --workspace
cargo test --workspace --locked
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check

# New CLI flag — single pass then exits.
target/debug/weather-bot --once

# Kill switch demo:
touch ./KILL && cargo run -p weather-bot
# every pass logs "kill switch active; skipping strategy pass"
rm ./KILL
```

## Recommended next actions (when you're up)

In priority order, mapped to ROADMAP.md's near-term list:

1. **Item 5 — station-table reconciliation** is now the cleanest unblocked
   item. The METAR research confirmed all 5 ICAOs match the CLI station,
   so the only diff to surface is rounding / preliminary-vs-final CLI.
2. **Item 7 — paper-trade mode** is gated on the design decision in the
   research doc (demo vs local mock). Pick one before writing code; both
   are reasonable but the choice affects the next two PRs.
3. **Item 11 — ECMWF blending** is now genuinely cheap. The research doc
   confirms the existing parser handles it unchanged. One small new
   `fetch_ecmwf_ifs025` method, one tiny `(GefsClient, EcmwfClient)`
   blender, done.
4. **Items 1–3 — operational** (start dry-run on a VPS; replay daily). The
   `--once` flag makes the cron version of "replay daily" trivial; the
   long-running version is unchanged.

What I deliberately did *not* touch:

- **Item 4 — historical Kalshi candle backfill.** The research found the
  endpoint, but actually wiring it through the replay binary is its own
  PR-sized chunk. Better to land that intentionally with you awake than
  rush it.
- **Item 8 — wiring the executor into the strategy loop.** Gated on
  paper-mode (item 7). The kill switch + cooldown + bankroll Kelly are
  the prereqs and they're done; this is the next chunk after paper mode.
- **Item 10 — METAR/ASOS observations.** Big chunk; needs the new
  `weather-observations` crate (or extension of `weather-forecast`) and
  a strategy-mode rework. The research doc gets the next session
  started, but I didn't begin the implementation.

## Test count delta

- Before: 133 unit + 5 ignored
- After: ~170 unit + 3 doc + 5 ignored

Net `+37` tests across the safety/quality items, all green.

## Files changed (high-level)

- `Cargo.toml` (no version bumps)
- `config/default.toml` — added `[kill_switch]` section, `bankroll_usd`
  in `[risk]`, expanded `[execution]` comment for the new `Paper` value
- `crates/weather-config/src/lib.rs` — `ExecutionMode::Paper`,
  `KillSwitchConfig`, `RiskConfig.bankroll_usd`, `Display`/`as_str()`
  for `ExecutionMode`
- `crates/weather-types/src/lib.rs` — `OrderbookQuote` moved here
- `crates/weather-strategy/src/lib.rs` — `decide()` takes `bankroll_usd`,
  rewritten Kelly math, `OrderbookQuote` re-exported
- `crates/weather-risk/src/{Cargo.toml,lib.rs}` — chrono prod dep,
  `last_signal_at`, `RejectReason::InCooldown`, `evaluate_at(now)`
- `crates/weather-pricing/src/lib.rs` — three doc-tests
- `crates/weather-bot/{Cargo.toml,src/main.rs}` — clap + rust_decimal_macros
  deps, `--once`, kill-switch wiring, SIGTERM handler, source-health
  wiring, risk-reject summary, mode-tagged JSONL
- `crates/weather-bot/src/{kill_switch.rs,source_health.rs}` — new modules
- `crates/weather-bot/src/decision_log.rs` — `mode` field on
  `DecisionRecord`, `record_from` takes `ExecutionMode`
- `ROADMAP.md` — checked off the items that landed; reasoning preserved
- `devlog.md` — new "2026-05-05 — Safety stack" entry
- `docs/research/{f64-money-audit,kalshi-historical-candles,metar-observations,paper-trade-mode,ecmwf-open-meteo}.md` — new
- `docs/MORNING-SUMMARY.md` — this file
