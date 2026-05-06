# Roadmap

A working roadmap for the Kalshi Weather Bot. Originally built on the
**edge framework** (PR #2). The May-2026 sweep (PRs #5–#16) closed every
item on the original "near-term" priority list and most of Phases 0/1/2/3/5
/6. The bot now runs end-to-end with GEFS ensemble σ, position/exposure
caps, RSA-PSS-SHA256 order auth (kill-switched), JSONL decision logging,
and a calibration backtest harness.

This file tracks **what's next**, not what's done. The narrative for shipped
work lives in [`devlog.md`](devlog.md); the architecture overview lives in
[`README.md`](README.md). Treat checkboxes here as the live backlog. Items
roughly land in the order listed within a phase, but a phase can run partially
in parallel with its neighbours where dependencies allow.

> Status legend: `[ ]` not started · `[~]` in progress · `[x]` done · `[?]`
> open question / needs decision before work starts.

---

## Where we are today (post-replay/NWS parser fix)

Anchored to current `main` so this section ages well — verify with `cargo
test --workspace` and a quick repo skim before reusing.

- **11-crate** Rust workspace (`weather-backtest` joined the original ten);
  `cargo build --workspace` clean; `cargo test --workspace --locked` runs
  **133 unit tests + 5 ignored** live-network integration tests. CI
  enforces `fmt --check`, `clippy -D warnings`, `build --locked`,
  `test --locked` on every PR via [`.github/workflows/ci.yml`](.github/workflows/ci.yml).
- `weather-scanner`: paginating, 429-retrying Kalshi `/markets` client +
  ticker parser pinned to live demo-api responses. Also exposes a recent
  Kalshi `/series/{series}/markets/{ticker}/candlesticks` fetcher for replay
  diagnostics.
- `weather-forecast`: four sources, all verified live before parsers were
  written.
  - `NwsClient` — 7-day deterministic point forecast (`/points` →
    `/gridpoints/.../forecast`). Captures NWS `generatedAt` for the
    post-update lockout gate, falls back to `updateTime`, and tolerates NWS
    sending both fields in the same payload.
  - `GefsClient` — Open-Meteo's 30-member GFS-0.5° ensemble; `daily_high_stats`
    / `daily_low_stats` aggregate per-(city, date) `(μ, σ)` inside the same
    standard-time window pricing uses.
  - `IemCliClient` — Iowa State's parsed JSON view of NWS Daily Climate
    Reports for realised settlement values.
  - `HistoricalForecastClient` — Open-Meteo's archived deterministic GFS
    forecasts, used by the calibrate binary.
- `weather-pricing`: Normal-CDF model with continuity correction and
  settlement-station validation. `price_market_with_sigma(threshold,
  forecast, sigma_override)` accepts a runtime σ; `price_market` is the
  thin wrapper that falls back to `sigma_for_horizon`. Output stamps
  `sigma_source` (`"gefs_ensemble"` / `"static"`).
- `weather-types::tempwindow`: half-open UTC settlement window
  `[date 00:00 LST, date+1 00:00 LST)` per Kalshi's standard-time rule.
- `weather-strategy`: fee-aware EV gate, both YES/NO sides, explicit
  `NoTrade` reasons including `PriceOutOfBand` (the `[$0.20, $0.92]`
  band that excludes blow-up-risky tail contracts) and `ForecastTooFresh`
  (the 30-min post-NWS-update lockout). Quarter-Kelly contract sizing.
- `weather-risk`: real `RiskManager` with per-market position cap, per-pass
  total-exposure cap, concurrent-positions cap. Wired into the strategy
  loop; `reset_for_pass` clears between passes.
- `weather-executor`: real RSA-PSS-SHA256 signer over `(timestamp ‖
  method ‖ path)`, KALSHI-ACCESS-{KEY,TIMESTAMP,SIGNATURE} headers, and
  the `POST /portfolio/orders` request shape. Hard-coded `never_send=true`
  short-circuits before HTTP. Disabling is an explicit
  `client.allow_real_sends()` code call — not a config knob, by design.
- `weather-bot`: end-to-end dry-run loop — scan, refresh NWS + GEFS on
  separate cadences, price (with GEFS σ when available, static fallback),
  gate (price band + lockout), risk-evaluate, JSONL decision log
  (`logs/decisions/YYYY-MM-DD.jsonl`), and one per-pass summary line with
  scanned/priced/traded/reason counts.
- `weather-backtest`: reads JSONL decisions, joins to IEM CLI outcomes,
  computes hit rate / Brier / log-loss / mean-bias / 10-bucket calibration
  histogram. Includes a synthetic-decision builder for Phase A
  pre-dry-run model calibration. Ships a `calibrate` binary
  (`cargo run -p weather-backtest --bin calibrate`) and a `replay` binary
  (`cargo run -p weather-backtest --bin replay -- logs/decisions`) that
  splits metrics by `sigma_source`, dedupes repeated dry-run trade emissions,
  and prints an immediate candle spread/fee mark for unique opportunities.
- `weather-monitor`: still a placeholder; the WebSocket-delta upgrade is
  deferred until polling latency demonstrably matters.

Anything that contradicts the above means the README/devlog/code drifted —
trust the code first and update this file.

---

## Phase 0 — Baseline stability and observability

The bot already runs, but we have no way to tell whether the model is *good*.
Before we add forecast sources or build out execution, get a clean signal on
what the bot already says.

- [x] **Persist every dry-run decision** to a JSONL file
  (`logs/decisions/YYYY-MM-DD.jsonl`). PR #5; schema extended in PRs
  #11 (threshold fields) and #16 (`sigma_source`).
- [ ] **Add a `--once` / single-pass CLI flag** to `weather-bot` so we can run
  the pipeline as a script in CI / cron without holding the process open.
  Today the bot only runs as a long-lived loop, which is awkward for offline
  evaluation.
- [ ] **Structured JSON logging path validated end-to-end.** `LoggingConfig`
  already supports `json_output`, but no consumer reads it yet — confirm fields
  are stable enough to tail into a log aggregator without further parsing
  tweaks.
- [x] **Per-pass summary log line:** N markets scanned, N priced, N gated by
  reason (`EdgeBelowMin`, `EvBelowGate`, `SpreadTooWide`, `NoOrderbook`,
  `PriceOutOfBand`, `ForecastTooFresh`, `UnknownCity`). Implemented in
  `weather-bot::PassSummary`.
- [x] **CI: `cargo build`, `cargo test --workspace`, `cargo fmt --check`,
  `cargo clippy -- -D warnings`** on PRs to `main`. PR #6 added the workflow
  + a fmt/clippy sweep over the existing tree.
- [ ] **Smoke test in CI** that boots the bot with a stub Kalshi server (or
  recorded fixtures) and asserts the strategy loop runs at least one pass
  without panicking. Catches integration breakage that unit tests miss.

> Why first: every later phase needs a way to *measure* its impact. Decision
> logs are the substrate for backtests, calibration, and Phase-9 dashboards.

---

## Phase 1 — Settlement data and station/source validation

The model compares NWS forecasts to a Kalshi-settled value. If those two
sources ever diverge — different station, different rounding, different
standard-time interpretation — every "edge" the bot sees is noise.

- [x] **Pull historical CLI reports** for the 5 v1 stations (KNYC, KMDW,
  KLAX, KMIA, KAUS) via Iowa State's parsed JSON view. Lives in
  `weather-forecast::cli`; PR #7. Note Chicago = KMDW (Midway), not KORD —
  fixed before the original sweep landed.
- [ ] **Resolved-market ingestion + settlement reconciliation.** Pull the last
  ~30 days of resolved KXHIGH/KXLOW markets from Kalshi
  (`/markets?status=settled` or `/events`), persist the resolved settlement
  value on the market/row shape, and compare each settlement to our parsed IEM
  CLI high/low. Any drift > 0 is a bug in our station table or parser.
- [ ] **Preliminary-vs-final CLI gating.** Kalshi's help-center notes that
  the bot may need to wait for the *final* CLI in cases of revision. Decide
  whether we trust preliminary CLI for our own settlement-day reasoning, and
  document the policy.
- [?] **Add more cities.** v1 hard-codes 5. Whichever city we add next needs
  its CLI station verified the same way — the table at
  `crates/weather-types/src/cities.rs` is intentionally tiny so adding a
  wrong row is loud.

---

## Phase 2 — Forecast / data ingestion

The model is currently fed by NWS only. Replacing fixed σ with a real
ensemble dispersion is where most of the model's edge will come from.

- [x] **NOAA GEFS ensemble (Open-Meteo path).** PR #10 added the fetcher
  + per-(city, date) `daily_high_stats` / `daily_low_stats`. PR #15 wired
  it into pricing — `price_market_with_sigma` now uses ensemble σ when
  available, falls back to the static table otherwise. PR #16 stamps
  `sigma_source` on every JSONL row so the backtest can split metrics.
  AWS `noaa-gefs-pds` (rigorous, GRIB2-parsing) path is still open — only
  needed if Open-Meteo rate-limits or we want non-temperature variables.
- [ ] **ECMWF / Open-Meteo ECMWF.** Open-Meteo's ensemble endpoint
  supports `models=ecmwf_ifs025` — second source for the same pricing
  path, blended with GEFS. Fetcher pattern is identical to `GefsClient`;
  blender is one weighted-σ helper.
- [ ] **METAR / ASOS observations** for *intra-day* nowcast updates. On
  settlement day, by the time it's 2pm local the bot can read the running
  high directly from observations and update P(YES) materially. This is the
  biggest potential edge for thin late-day markets. NWS exposes
  `/stations/{ICAO}/observations` directly; no new auth, no new dep.
- [ ] **Forecast cache abstraction.** Today `weather-bot::ForecastCache`
  and `EnsembleCache` are two parallel `HashMap<city_code, ...>` stores.
  As we add ECMWF / METAR we want `HashMap<(source, city_code), ...>` with a
  single accessor for "best available forecast inputs for this city/day" so
  the pricing layer doesn't grow source switches.
- [ ] **Source health checks surfaced in summaries.** If a source's last
  successful refresh is older than 2× its expected interval, log a
  `SourceStale` warning, skip that source rather than feeding stale data to
  pricing, and expose stale-source counts in the pass summary / replay report.

> Each source lands as a separate fetcher behind a `ForecastSource` trait
> (`fn fetch(city) -> impl Future<Output = Result<Forecast>>`). Don't add a
> source without a unit test pinned to a real fixture — same discipline as
> the Kalshi scanner.

---

## Phase 3 — Backtesting framework

We can't trust σ calibration or strategy thresholds without a way to replay
history. This becomes possible once Phase 1 + Phase 2 land enough data.

- [~] **Replay loop.** PR #11 added the `weather-backtest` crate: read
  JSONL decisions, join to CLI outcomes, compute hit rate / Brier /
  log-loss / mean-bias / 10-bucket calibration. PR #13 added the
  Open-Meteo historical-forecast fetcher + a synthetic-decision builder
  for Phase A pre-dry-run calibration. PR #14 shipped the `calibrate`
  binary that wires fetchers → decisions → metrics. The `replay` binary now
  reads `logs/decisions/*.jsonl`, splits outcome metrics by `sigma_source`,
  dedupes repeated dry-run Trade emissions into unique opportunities, and
  joins recent Kalshi candles for an immediate spread/fee mark. **Still
  missing**: multi-horizon replay (the historical-forecast archive is keyed
  on valid date, not issue time), older `/historical/.../candlesticks`
  backfill, and fill/lifecycle-aware realised P&L.
- [ ] **Opportunity-level resolved replay.** The immediate candle mark already
  dedupes repeated dry-run Trade emissions, but resolved calibration metrics
  still aggregate over raw JSONL rows. Add a replay mode that groups or
  weights repeated emissions by unique opportunity so one signal repeated
  every 5 seconds doesn't dominate hit rate / Brier / calibration buckets.
- [ ] **Daily dry-run report.** Add a lightweight report command over
  `logs/decisions/*.jsonl`: row count, unique opportunities, source mix,
  priced count, trade candidates, top no-trade reasons, stale/fetch failures,
  and immediate spread/fee marks. This can live inside the existing `replay`
  binary.
- [x] **Recent Kalshi candles.** `weather-scanner::candles` fetches
  `/series/{series_ticker}/markets/{ticker}/candlesticks`, parses trade OHLC
  and top-of-book YES bid/ask OHLC, and powers the replay binary's immediate
  candle mark. This is a dry-run friction diagnostic, not realised P&L.
- [ ] **Historical Kalshi candle backfill.** Markets older than Kalshi's
  recent candle window need `GET /historical/markets/{ticker}/candlesticks`
  or an equivalent archive path before we can run canonical long-window P&L
  regressions.
- [ ] **Walk-forward calibration.** Use a rolling window (e.g. last 60 days)
  to fit σ per (city, horizon) and evaluate on the next 14, sliding forward.
  This is what produces a real `sigma_for_horizon` replacement.
- [x] **Headline metrics** per replay: hit rate, mean Brier, mean log loss,
  mean signed bias, 10-bucket calibration histogram. PR #11. The replay
  binary now prints those by `sigma_source` once resolved CLI outcomes are
  available.
- [ ] **Regression suite.** Pick a fixed historical window and a fixed config
  and check that future code changes don't tank P&L on the canonical replay.
  Run as a CI job (slow tier).

---

## Phase 4 — Model calibration

This is where the bot stops being a toy. Each item is gated on Phase 3's
infrastructure existing.

- [ ] **Per-city σ.** Coastal cities (MIA, LAX) have different forecast-error
  distributions than continental ones (CHI, AUS). Single national σ is a
  defensible v1 placeholder; per-city σ is table stakes for v2.
- [ ] **Per-horizon σ from realized errors.** The current `1.6 → 5.0` ramp is
  hand-calibrated to NWS published verification. Replace with empirical σ
  fit on the last N months of (forecast, observation) pairs.
- [ ] **Per-season σ.** Forecast skill in summer ≠ winter. Fit either by
  meteorological season or by a sliding-window approach.
- [ ] **Non-Gaussian tails.** NWS errors are reasonably Gaussian in the bulk
  but have heavier-than-Normal tails. Once we have data, fit a Student-t or
  mixture and re-run the backtest to see whether tail mass changes the EV
  gate's decisions on extreme strikes.
- [ ] **Bin-market support.** `parse_weather_threshold` rejects
  `strike_type=between` markets today. Adding them is Normal-CDF
  `Φ((upper - μ)/σ) − Φ((lower - μ)/σ)` — straightforward once the
  one-sided model is calibrated.
- [?] **Precipitation / snow markets.** The current crate name is
  `weather-pricing` not `temp-pricing` for a reason, but precip is a
  zero-inflated, heavy-tailed distribution and a different problem entirely.
  Decide whether v2 includes precip or stays high-temp / low-temp only.

---

## Phase 5 — Market microstructure & execution

`weather-executor` now has the auth path and request shape, kill-switched
behind a hard-coded `never_send=true`. Live trading still needs lifecycle
tracking + microstructure awareness so the bot doesn't get adversely
selected on every fill.

- [x] **Kalshi RSA-PSS-SHA256 auth.** PR #9 added `KalshiSigner` (PKCS#8
  + PKCS#1 PEM loaders), `signing_string(ts, method, path)`, base64
  signature, and the three KALSHI-ACCESS-* headers via
  `AccessHeaders::apply(builder)`.
- [x] **`POST /portfolio/orders` + dry-run/live opt-in.** PR #9 added
  `OrderRequest::from_signal`, `KalshiOrderClient::place_order`, and the
  hard-coded `never_send=true` short-circuit. Disabling is an explicit
  `client.allow_real_sends()` code call — not a config knob, by design.
- [x] **Post-only / limit at the resting opposite-side price.**
  `OrderRequest::from_signal` always sets `post_only = true` and uses the
  signal's `limit_price` (which the strategy populates with the opposite-
  side ask).
- [ ] **Wire the executor into the strategy loop.** Today the bot has the
  executor crate available but `run_strategy_pass` doesn't actually call
  `place_order`. Needs the same `Decision::Trade` → executor handoff the
  risk layer already has.
- [ ] **Order lifecycle tracking.** Track open orders via
  `/portfolio/orders` and reconcile with our local "intended" state every
  pass. Cancel-and-reprice if our model probability moves > X bps, the book
  shifts, or our estimated queue position becomes unattractive.
- [ ] **Stale quote detection.** If the orderbook timestamp is older than 2×
  poll interval, treat as `NoOrderbook` rather than trade against a stale
  book. This is the cheap version of WebSocket integration.
- [ ] **Kalshi WebSocket (`/trade-api/ws/v2`) with `orderbook_delta`.**
  Already noted in the README as the deferred upgrade. Worth doing when the
  bot is regularly trading > 20 markets and REST polling latency matters.
  This is finally what `weather-monitor` is supposed to host.
---

## Phase 6 — Risk management

`weather-risk` is real. The strategy loop now runs every Trade signal
through `RiskManager::evaluate` before the executor would see it.

- [x] **Position size cap.** Per-market `max_position_size_usd` clips
  contract count. PR #8.
- [x] **Total exposure cap.** Per-pass running sum vs
  `max_total_exposure_usd`; further clip or reject. PR #8.
- [x] **Concurrent positions cap.** Hard cap on signal count per pass via
  `max_concurrent_positions`. PR #8.
- [ ] **Per-market cooldown.** After a fill (or a cancel), don't re-emit a
  signal for the same market for `per_market_cooldown_secs`. Prevents the bot
  thrashing on tiny price wiggles. Config exists; cooldown logic doesn't.
- [ ] **Dry-run intended-position state.** Before live execution, keep a local
  "already emitted this opportunity" ledger so dry-run logs don't repeat the
  same signal every pass. This is the dry-run precursor to real order/position
  state and should share keys with replay's opportunity dedupe.
- [ ] **Per-city / per-day exposure cap.** Five YES bets on a hot day in NYC
  are not five independent trades — they're correlated. Add a cap on
  `(city, date)` aggregate notional.
- [ ] **Bankroll-fraction sizing.** Today Kelly is computed against an
  implicit bankroll of $1; the contract count comes out of the strategy as
  a hint and is then bounded by `max_position_size_usd`. Refactor so Kelly
  and the cap interact correctly: Kelly determines a *fraction* of the
  configured bankroll, then the cap clamps that to the per-market dollar
  ceiling.
- [ ] **Kill switch.** A file flag (`./KILL` exists) or env var
  (`WEATHER_BOT_KILL=1`) that the bot reads each pass and, if set, cancels
  all open orders and refuses to place new ones. (The executor's
  `never_send` is the *static* kill-switch; this would be the *dynamic*
  one the operator can toggle without redeploying.)
- [ ] **Paper / live mode separation surfaced everywhere.** Logs, metrics,
  decision JSONL — every line should carry `mode = dry_run | paper | live`
  so backtests and live runs don't get mixed.

> Risk-adjacent gates that aren't strictly the risk layer's job, but
> landed alongside it:
>
> - **Price band gate** (`[$0.20, $0.92]`): excludes tail-bracket
>   contracts where one losing trade wipes out months of small wins.
>   Lives in `weather-strategy::decide`. PR #12.
> - **NWS-update lockout (30 min)**: sit out the first 30 min after each
>   NWS forecast issue (`generatedAt`) so arbitrage bots don't pick us
>   off. PR #12.

---

## Phase 7 — Production operations

Ship-readiness, not feature work. Each item is small but non-optional before
running with real money.

- [ ] **Config layering.** `weather-config` already supports env override
  with `__`. Add `config/local.toml` (gitignored) for per-operator tweaks
  and an explicit "this overrides that" precedence note in the README.
- [ ] **Dry-run process supervision.** systemd, launchd, or equivalent with
  restart-on-failure. The immediate goal is uninterrupted JSONL collection;
  live deployment hardening can build on the same unit later.
- [ ] **Operator runbook.** Document the daily workflow: start/check the bot,
  read pass summaries, run replay with `--skip-outcomes`, rerun after CLI
  reports publish, and distinguish immediate spread/fee marks from resolved
  calibration.
- [ ] **Minimal alerting.** At minimum: bot exits non-zero, source fetches fail
  for more than N minutes, and settlement-day markets have no orderbook. Live
  order alerts wait until live execution exists.

---

## Phase 8 — Research questions and open decisions

Items that are not yet ready to be tickets — they need a written decision
first. Keep this section short; once a question has a concrete acceptance
criterion, move it into the phase backlog above.

- [?] **Single-source vs blended forecast.** Three independent sources (NWS,
  GEFS, ECMWF) → does the model use the GEFS ensemble σ directly, or blend
  point forecasts with a per-source weight learned from history? The latter
  is more powerful, but harder to debug. Decide before Phase 4.
- [?] **Settlement-day rules.** Kalshi closes weather markets before the CLI
  is even issued. Is the bot allowed to take new positions in the last hour
  before close, or do we cut off earlier to avoid being filled on a stale
  forecast? Probably city-specific.
- [?] **Concurrency model for the executor.** Today the strategy loop is
  serial across markets. Do we keep it that way (predictable; easy to reason
  about rate limits) or split into per-city tasks once we're trading 50+
  markets?
- [?] **Module C — forecast-revision momentum.** The current pricing path
  is essentially Module A (ensemble divergence: model `P(YES)` vs market
  price). Phase 2's METAR item is Module B (intraday observation lock).
  Module C is a third, structurally uncorrelated signal: when the GEFS
  06z run shifts ensemble mean by >X°F vs the 00z run *and* the market
  hasn't moved, lean toward the new run. Cheap once GEFS fetching
  exists — needs run-time-indexed caching (we currently keep only the
  latest run per city). Worth scaffolding once the dry-run + backtest
  prove Module A has measurable edge; doing it before is premature.
- [?] **Regime-aware σ.** Forecast skill collapses around fronts and storms.
  Decide whether a cheap proxy (spread between deterministic and ensemble
  means, ensemble skew, or run-to-run volatility) is enough to inflate σ
  before trying a heavier model.
- [?] **Python sidecar for Phase 4 fitting.** Closed-form Normal-CDF is
  fine in Rust; non-Gaussian tail fits, Bayesian σ updates, and
  gradient-boosted bracket probabilities are not — every Python ML
  library exists for a reason. Decision to make *now*: when Phase 4
  starts, fit parameters in a Python sidecar (read JSONL + CLI, write
  fitted-σ table as JSON), and have the Rust pricing crate consume
  that table. Don't hand-roll fitting code in Rust just because the
  rest of the bot is in Rust. Keep the hot path Rust, the fitting cold
  path Python.

---

## Phase 9 — Quality, refactors, and tech debt

Things that are fine today but will matter when the codebase doubles.

- [ ] **Unify orderbook quote types.** `OrderbookQuote::from_market` lives in
  `weather-strategy`; the same data flows through `weather-scanner`. One
  shared type in `weather-types` would be cleaner.
- [ ] **`weather-monitor` either becomes the WebSocket layer (Phase 5) or
  gets deleted.** The empty crate is technical debt either way; the README
  notes it as v2's landing pad.
- [x] **Clippy clean.** PR #6 fixed the existing drift (boxed
  `EvBreakdown` in `Decision::Trade` to fix `large_enum_variant`,
  simplified two scanner patterns) and CI now enforces
  `clippy --workspace --all-targets -- -D warnings` on every PR.
- [ ] **Audit money math types.** Prices, fees, notional exposure, and P&L
  should use `Decimal`; probabilities and σ can stay `f64` where appropriate.
  Spot-check replay and executor paths before live/paper trading.
- [ ] **Doc-tests on the pricing math.** A doctest on `price_market` that
  checks "μ = T → P ≈ 0.5" is exactly the kind of regression we want when
  someone "improves" the model later.

---

## Near-term next tasks (priority-ordered, current main)

The original 7-step priority list (decision-log → CI → CLI fetcher → risk
caps → executor auth → GEFS fetcher → backtest replay) all merged in PRs
#5–#16. The follow-up replay/NWS parser work added the dry-run replay binary,
recent candle marks, repeated-emission dedupe, and a tolerant NWS timestamp
parser. The next sequence shifts focus from "build the substrate" to "prove
edge exists" and "make it safe to actually trade".

> **Note on the dry-run** *(Perplexity feedback, 2026-05-04)*: 14 days
> against 5 cities is realistically only 20–80 trade-eligible decisions
> after the gates fire — far too few to fill the calibration histogram
> with statistical power. Treat the dry-run as a **smoke test** (does
> the bot run? do gates fire? are sources healthy?). The actual
> **calibration evidence** comes from running the synthetic-decision
> backtest harness over months of historical data via the `calibrate`
> binary. Both. Don't conflate them.

1. **Start the dry-run today** — `cargo run -p weather-bot`, ideally
   under systemd on a VPS so it's not waiting on your laptop. Every day
   of data we don't have is a day of decisions we're making blind.
2. **Replay dry-run logs daily.** Use
   `cargo run -p weather-backtest --bin replay -- logs/decisions --skip-outcomes`
   while same-day markets are unresolved. Treat the immediate candle section
   as a spread/fee friction mark only; it dedupes repeated dry-run emissions
   and is not realised strategy P&L.
3. **Replay resolved outcomes by `sigma_source`.** Once CLI reports are
   available for the resolution dates in the JSONL, rerun replay without
   `--skip-outcomes`. This is the first direct answer to "did GEFS σ help?"
   on actual dry-run rows.
4. **Opportunity-level resolved metrics.** Before treating replay calibration
   as model evidence, add grouping/weighting so repeated dry-run emissions
   don't dominate Brier/log-loss/calibration buckets.
5. **Dry-run intended-position state.** Stop the bot from re-emitting the same
   opportunity every pass. This keeps logs readable and mirrors the state
   live order lifecycle tracking will eventually need.
6. **Historical Kalshi candle backfill.** The recent `/candlesticks` fetcher
   exists, but older windows need Kalshi's historical candle endpoint (or an
   archive) before we can compute canonical long-window net-of-fee expectancy.
7. **Reconcile our station table against settled Kalshi markets.** Pull
   the last ~30 days of resolved KXHIGH/KXLOW from Kalshi
   (`/markets?status=settled`), compare each settlement to the IEM CLI
   high/low. Drift > 0 is a station-table bug. Phase 1's open item.
8. **Dynamic kill switch.** *Required before any live trading.* The
   static `never_send=true` in the executor is a development guard —
   it requires a code edit to disable. The *dynamic* kill switch is for
   the operator to halt the bot in 5 seconds without redeploying:
   - file flag `./KILL` checked at the top of every pass
   - `SIGTERM` handler that cancels open orders
   - env var `WEATHER_BOT_KILL=1`
   - cumulative-drawdown soft kill: bot itself refuses new orders if
     down >X% of bankroll in 24h. (Static kill is for you protecting
     the bot; soft kill is for the bot protecting you from yourself.)
   All four, because they fail in different ways.
9. **Paper-trade mode.** *Required before any live trading.* Add a third
   mode between dry-run and live: same code path as live, but hits a
   Kalshi paper endpoint or a local mock that simulates fills against
   the real orderbook. Reveals lifecycle bugs (cancel-and-reprice,
   queue position, stale quote, partial fills, rejections, timeouts)
   that dry-run can't because dry-run never actually places anything.
10. **Wire the executor into the strategy loop.** Now (after items 8-9
   land) the bot can actually trade. `Decision::Trade` →
   `OrderRequest::from_signal` → `KalshiOrderClient::place_order`,
   gated by `cfg.execution.mode` and `never_send`.
11. **Per-market cooldown + bankroll-fraction Kelly.** Phase 6's open
   items. Both small. Lets the bot run unattended without thrashing.
12. **METAR / station observations.** Phase 2's "biggest potential
    edge" item — same-day intraday "lock" trades. Real engineering
    (new fetcher, new strategy mode), but per Perplexity's research,
    this is where retail edge actually lives.
13. **ECMWF blending.** Second source for the same pricing path. Cheap
    once GEFS is wired (same Open-Meteo client, just a different
    `models=` param).

Items 1–7 unblock the empirical question. Items 8–9 are the safety
infrastructure that has to land *before* `never_send=false`. Item 10
turns the bot from a "decision logger" into a "trader" — only do it
after 8–9 give us a way to halt cleanly and a way to find lifecycle
bugs without spending money.

---

## How to keep this file useful

- Update checkboxes as items land — the value of this file is that the next
  reader trusts it.
- When a phase is mostly done, fold its narrative into `devlog.md` and trim
  the section here to the still-open items.
- Don't add an item unless you can describe (a) why it's needed and (b) how
  you'd know it's done. Vague items rot.
- New phases go at the end; existing phase numbers don't shift, so external
  references (PRs, issues, devlog entries citing "Phase 3") keep working.
