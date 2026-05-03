# Roadmap

A working roadmap for the Kalshi Weather Bot. Built on top of the merged
**edge framework** (PR #2 → `main`): probabilistic Normal-CDF pricing,
horizon-indexed σ, settlement-station validation for the v1 cities (NY, CHI,
LAX, MIA, AUS), DST-aware standard-time settlement window, Kalshi fee model,
fee + spread + safety-buffer EV gate, and dry-run decision logging.

This file tracks **what's next**, not what's done. The narrative for shipped
work lives in [`devlog.md`](devlog.md); the architecture overview lives in
[`README.md`](README.md). Treat checkboxes here as the live backlog. Items
roughly land in the order listed within a phase, but a phase can run partially
in parallel with its neighbours where dependencies allow.

> Status legend: `[ ]` not started · `[~]` in progress · `[x]` done · `[?]`
> open question / needs decision before work starts.

---

## Where we are today (post-PR #2)

Anchored to current `main` so this section ages well — verify with `cargo test
--workspace` and a quick repo skim before reusing.

- 10-crate Rust workspace; `cargo build --workspace` and `cargo test
  --workspace` clean (36 unit tests across types/pricing/strategy/scanner/
  forecast/config).
- `weather-scanner`: paginating, 429-retrying Kalshi `/markets` client +
  ticker parser pinned to live demo-api responses.
- `weather-forecast`: NOAA NWS two-step flow (`/points` → `/gridpoints/.../
  forecast`).
- `weather-pricing`: Normal-CDF model with horizon-indexed σ (1.6°F at d0 →
  5.0°F at d7), continuity correction, settlement-station validation against
  the 5-city table.
- `weather-types::tempwindow`: half-open UTC settlement window
  `[date 00:00 LST, date+1 00:00 LST)` per Kalshi's standard-time rule.
- `weather-strategy`: fee-aware EV gate, both YES- and NO-side evaluation,
  explicit `NoTrade` reasons, quarter-Kelly contract sizing, fee model from
  `multiplier * 0.07 * p * (1-p)` rounded up to the cent.
- `weather-bot`: end-to-end dry-run loop — scan, refresh forecasts on demand,
  price, gate, log decisions.
- **Stubs** (still TODO-only): `weather-executor` (Kalshi RSA-PSS-SHA256 +
  order placement), `weather-risk` (caps, cooldowns), `weather-monitor` (held
  open as the v2 WebSocket landing pad).

Anything that contradicts the above means the README/devlog/code drifted —
trust the code first and update this file.

---

## Phase 0 — Baseline stability and observability

The bot already runs, but we have no way to tell whether the model is *good*.
Before we add forecast sources or build out execution, get a clean signal on
what the bot already says.

- [ ] **Persist every dry-run decision** to a JSONL or SQLite file
  (`logs/decisions/YYYY-MM-DD.jsonl` is the obvious shape). One row per market
  per pass: ticker, timestamp, yes_bid/ask, model_p, σ, horizon, fee, raw
  edge, net EV, decision, reason. Without this we can't compare model-implied
  probabilities to settled outcomes a week later.
- [ ] **Add a `--once` / single-pass CLI flag** to `weather-bot` so we can run
  the pipeline as a script in CI / cron without holding the process open.
  Today the bot only runs as a long-lived loop, which is awkward for offline
  evaluation.
- [ ] **Structured JSON logging path validated end-to-end.** `LoggingConfig`
  already supports `json_output`, but no consumer reads it yet — confirm fields
  are stable enough to tail into a log aggregator without further parsing
  tweaks.
- [ ] **Per-pass summary log line:** N markets scanned, N priced, N gated by
  reason (`EdgeBelowMin`, `EvBelowGate`, `SpreadTooWide`, `NoOrderbook`,
  `UnknownCity`). Currently each market logs individually and the operator has
  to count.
- [ ] **CI: `cargo build`, `cargo test --workspace`, `cargo fmt --check`,
  `cargo clippy -- -D warnings`** on PRs to `main`. The repo has no CI yet;
  this is cheap and prevents the next regression.
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

- [ ] **Pull historical CLI reports** for the 5 v1 stations (KNYC, KORD,
  KLAX, KMIA, KAUS). Iowa State IEM (`mesonet.agron.iastate.edu/wx/afos`)
  hosts CLI text products; ASOS one-minute / hourly observations are also
  on IEM. Need a small fetcher in `weather-forecast` (or a sibling crate) that
  parses CLI to a `(date, station, high_f, low_f)` record.
- [ ] **Reconcile CLI vs Kalshi settlement** for past markets: pull the last
  ~30 days of resolved KXHIGH/KXLOW markets from Kalshi
  (`/markets?status=settled` or `/events`), compare each settlement value to
  our parsed CLI high/low. Any drift > 0 is a bug in our station table or
  parser.
- [ ] **Surface a `settlement_value` field on `KalshiMarket`** when it's
  resolved, so backtests can join model probability → realized outcome
  without re-fetching.
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

- [ ] **NOAA GEFS ensemble.** ~30 perturbed members, 0.25° / 0.5° grids,
  3-hourly out to 16 days. Available on AWS Open Data
  (`noaa-gefs-pds`). Job: fetch the latest run's 2 m temperature for each
  city's grid cell, derive `(μ, σ)` per (city, day). This *replaces* the
  current `sigma_for_horizon` table for in-horizon days.
- [ ] **ECMWF / Open-Meteo ECMWF.** Open-Meteo's free API exposes a
  blended-model and an ECMWF-only forecast. Useful as a second model to
  blend with NWS / GEFS. Lower priority than GEFS for ensemble σ but cheaper
  to integrate.
- [ ] **METAR / ASOS observations** for *intra-day* nowcast updates. On
  settlement day, by the time it's 2pm local the bot can read the running
  high directly from observations and update P(YES) materially. This is the
  biggest potential edge for thin late-day markets.
- [ ] **Forecast cache abstraction.** Today `weather-bot::ForecastCache` is a
  `HashMap<city_code, (Forecast, ts)>`. As we add sources we want
  `HashMap<(source, city_code), ...>` with a single `blended_forecast(city,
  day)` accessor in `weather-pricing` so the pricing layer doesn't grow source
  switches.
- [ ] **Source health checks.** If a source's last successful refresh is
  older than 2× its expected interval, log a `SourceStale` warning and *skip*
  it rather than feeding stale data to the blend.

> Each source lands as a separate fetcher behind a `ForecastSource` trait
> (`fn fetch(city) -> impl Future<Output = Result<Forecast>>`). Don't add a
> source without a unit test pinned to a real fixture — same discipline as
> the Kalshi scanner.

---

## Phase 3 — Backtesting framework

We can't trust σ calibration or strategy thresholds without a way to replay
history. This becomes possible once Phase 1 + Phase 2 land enough data.

- [ ] **Replay loop.** Given a date `D`, a set of `(forecast snapshot at D-h)
  for h in 0..7`, and the realized CLI outcome on `D`, compute what the bot
  *would have decided* at each horizon and what the P&L would have been at
  each Kalshi closing price.
- [ ] **Historical Kalshi prices.** Kalshi exposes `/markets/{ticker}/
  candles` for OHLC; check whether it's enough or whether we need to record a
  rolling orderbook snapshot ourselves going forward.
- [ ] **Walk-forward calibration.** Use a rolling window (e.g. last 60 days)
  to fit σ per (city, horizon) and evaluate on the next 14, sliding forward.
  This is what produces a real `sigma_for_horizon` replacement.
- [ ] **Headline metrics** per replay: hit rate, avg fee paid, avg net EV vs
  realized P&L, Brier score / log-loss for the model probability vs the
  outcome, calibration plot (predicted P bucketed vs realized hit rate).
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

`weather-executor` is a stub today. Live trading needs auth + order placement,
plus enough microstructure awareness that the bot doesn't get adversely
selected on every fill.

- [ ] **Kalshi RSA-PSS-SHA256 auth.** Load PEM at startup, sign
  `timestamp + method + path`, send `KALSHI-ACCESS-{KEY,TIMESTAMP,SIGNATURE}`
  headers. Demo env first. The crate doc-comment in
  `crates/weather-executor/src/lib.rs` already lists the spec.
- [ ] **`POST /portfolio/orders` + dry-run/live opt-in.** Mirror the
  polymarket bot's pattern: even with `mode = "live"` and creds set, log the
  payload before sending until manually flipped on.
- [ ] **Post-only / limit at the resting opposite-side price.** We already
  log `limit_price = opposite ask` in the dry-run line; make sure live orders
  use the same and don't accidentally cross.
- [ ] **Order lifecycle tracking.** Track open orders via
  `/portfolio/orders` and reconcile with our local "intended" state every
  pass. Cancel-and-reprice if our model probability moves > X bps or the book
  shifts.
- [ ] **Stale quote detection.** If the orderbook timestamp is older than 2×
  poll interval, treat as `NoOrderbook` rather than trade against a stale
  book. This is the cheap version of WebSocket integration.
- [ ] **Kalshi WebSocket (`/trade-api/ws/v2`) with `orderbook_delta`.**
  Already noted in the README as the deferred upgrade. Worth doing when the
  bot is regularly trading > 20 markets and REST polling latency matters.
  This is finally what `weather-monitor` is supposed to host.
- [ ] **Queue position awareness.** A post-only YES @ 0.50 sitting behind 200
  contracts is a *different* order than one with 0 ahead. We won't model this
  perfectly, but at least track our rank and let the bot pull-and-reprice if
  the book in front of us evaporates without our order filling.

---

## Phase 6 — Risk management

`weather-risk` is also a stub. Strategy emits a contract count today; nothing
checks it against caps.

- [ ] **Position size cap.** Enforce `max_position_size_usd` per market;
  shrink contract count to fit. Already configured in
  `config/default.toml`, just unused.
- [ ] **Total exposure cap.** Sum of `(contracts * limit_price)` across open
  positions ≤ `max_total_exposure_usd`. Reject new signals that would breach
  it.
- [ ] **Per-market cooldown.** After a fill (or a cancel), don't re-emit a
  signal for the same market for `per_market_cooldown_secs`. Prevents the bot
  thrashing on tiny price wiggles.
- [ ] **Per-city / per-day exposure cap.** Five YES bets on a hot day in NYC
  are not five independent trades — they're correlated. Add a cap on
  `(city, date)` aggregate notional.
- [ ] **Concurrent positions cap** (`max_concurrent_positions`). Already
  configured, needs wiring.
- [ ] **Bankroll-fraction sizing.** Today Kelly is computed against an
  implicit bankroll of $1; the contract count comes out of the strategy as
  a hint and is then bounded by `max_position_size_usd`. Refactor so Kelly
  and the cap interact correctly: Kelly determines a *fraction* of the
  configured bankroll, then the cap clamps that to the per-market dollar
  ceiling.
- [ ] **Kill switch.** A file flag (`./KILL` exists) or env var
  (`WEATHER_BOT_KILL=1`) that the bot reads each pass and, if set, cancels
  all open orders and refuses to place new ones.
- [ ] **Paper / live mode separation surfaced everywhere.** Logs, metrics,
  decision JSONL — every line should carry `mode = dry_run | paper | live`
  so backtests and live runs don't get mixed.

---

## Phase 7 — Production operations

Ship-readiness, not feature work. Each item is small but non-optional before
running with real money.

- [ ] **Secrets handling.** Today `.env` carries `KALSHI_API_KEY_ID` +
  `KALSHI_PRIVATE_KEY_PATH`. Document the deploy story (systemd? Docker
  secret? cloud secret manager?) and make sure `.env` and the PEM are never
  in git history.
- [ ] **Config layering.** `weather-config` already supports env override
  with `__`. Add `config/local.toml` (gitignored) for per-operator tweaks
  and an explicit "this overrides that" precedence note in the README.
- [ ] **Process supervision.** systemd unit (or equivalent) with
  `Restart=on-failure`. The bot's main loop is single-process; if it panics
  we want it back up within seconds.
- [ ] **Alerting.** At minimum: an email or Slack webhook on (a) the bot
  exiting non-zero, (b) `mode = live` placed orders, (c) any
  `forecast.fetch_failed` lasting > N minutes, (d) settlement-day no-orderbook
  on a tracked market.
- [ ] **Dashboards.** Grafana or equivalent reading from the JSONL decision
  log: per-pass scan count, per-reason no-trade count, per-city σ, P&L if
  live, fee paid. Cheap version: a `cargo run -p weather-tools -- summary`
  that prints today's stats from the JSONL files.
- [ ] **Prometheus metrics endpoint.** `tracing` + `tracing-subscriber` can
  feed `metrics-exporter-prometheus`; counters for scans, decisions, errors
  per crate. Skip if not deploying anywhere with Prometheus.
- [ ] **Versioned config schema.** Once we're live, schema changes need a
  migration story. Add a `config_version` field and assert it on load.

---

## Phase 8 — Research questions and open decisions

Items that are not yet ready to be tickets — they need a written decision
first.

- [?] **Single-source vs blended forecast.** Three independent sources (NWS,
  GEFS, ECMWF) → does the model use the GEFS ensemble σ directly, or blend
  point forecasts with a per-source weight learned from history? The latter
  is more powerful, but harder to debug. Decide before Phase 4.
- [?] **Settlement-day rules.** Kalshi closes weather markets before the CLI
  is even issued. Is the bot allowed to take new positions in the last hour
  before close, or do we cut off earlier to avoid being filled on a stale
  forecast? Probably city-specific.
- [?] **Tax / accounting.** Live-trading P&L has tax implications. Out of
  scope for the bot itself but should be flagged in `README.md` before
  someone runs it on a real account.
- [?] **Concurrency model for the executor.** Today the strategy loop is
  serial across markets. Do we keep it that way (predictable; easy to reason
  about rate limits) or split into per-city tasks once we're trading 50+
  markets?
- [?] **σ that depends on synoptic regime.** Forecast skill collapses around
  fronts and storms. Can we cheaply detect "we're inside a regime change"
  and inflate σ? Probably needs a proxy like spread between deterministic
  and ensemble means.
- [?] **Bid/ask spread tightening.** As volume grows in Kalshi weather, the
  cheap `max_spread = 0.10` cutoff might be leaving easy edge on the table.
  Re-evaluate after backtests.

---

## Phase 9 — Quality, refactors, and tech debt

Things that are fine today but will matter when the codebase doubles.

- [ ] **Unify orderbook quote types.** `OrderbookQuote::from_market` lives in
  `weather-strategy`; the same data flows through `weather-scanner`. One
  shared type in `weather-types` would be cleaner.
- [ ] **`weather-monitor` either becomes the WebSocket layer (Phase 5) or
  gets deleted.** The empty crate is technical debt either way; the README
  notes it as v2's landing pad.
- [ ] **Clippy clean.** Currently no clippy enforcement. Adopt
  `clippy::pedantic` selectively + `-D warnings` as part of CI.
- [ ] **`Decimal` everywhere there's money.** The codebase is mostly already
  there, but spot-check that no `f64` is silently doing price math —
  especially in any new ensemble code in Phase 2.
- [ ] **Doc-tests on the pricing math.** A doctest on `price_market` that
  checks "μ = T → P ≈ 0.5" is exactly the kind of regression we want when
  someone "improves" the model later.

---

## Near-term next tasks (priority-ordered)

If a contributor (human or agent) picks the next thing off this roadmap, do
them in this order:

1. **Phase 0 → decision-log JSONL.** Cheapest, unlocks everything.
2. **Phase 0 → CI (build/test/fmt/clippy).** A single GitHub Actions
   workflow. Stops regressions before they're reviewed.
3. **Phase 1 → CLI report fetcher + reconciliation against the last 30 days
   of settled markets.** Validates the v1 station table, and produces the
   first "real" data the backtest will need.
4. **Phase 6 → minimal risk manager (position cap + total exposure).** Wire
   in the values that already sit in `config/default.toml`. Cheap, makes the
   bot safe to be left running unattended in dry-run for longer.
5. **Phase 5 → Kalshi RSA auth + `POST /portfolio/orders` against demo.**
   Behind a hard-coded "do not actually send" flag at first. Gets the auth
   path debugged before any of the risk / execution loop matters.
6. **Phase 2 → GEFS ensemble fetcher.** First real upgrade to the model.
   Replaces the hand-calibrated σ on in-horizon days.
7. **Phase 3 → backtest replay using the JSONL log + GEFS history + CLI
   data.** First quantitative check that the model edge is real.

After step 7 we'll know whether the bot has a measurable edge. Phases 4 and 5
(beyond auth) only make sense if the answer is yes.

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
