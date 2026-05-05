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

---

## 2026-05-03 — Sweep 1: the original 7-step priority list (PRs #5–#11)

A long arc that closed every item on the "near-term next tasks" list at the
bottom of `ROADMAP.md` as it stood post-PR-#2. Each task landed as its own
PR; below is the narrative + the decisions worth remembering. Mechanical
test counts and file paths live in the PR descriptions on GitHub.

### Decision: build the substrate before chasing edge

The original list interleaved "infrastructure" (decision log, CI,
backtest framework) with "data sources" (CLI, GEFS) with "execution"
(risk caps, RSA auth). Doing them as separate small PRs in priority
order — rather than rolling them into one giant "phase 1" branch —
turned out to be the right call. Each PR was independently mergeable,
each had its own CI signal once #6 landed, and the dependency graph
naturally surfaced order constraints (e.g., the boxing of
`Decision::Trade(Signal, EvBreakdown)` in PR #6 forced a follow-up to
PR #5's tests, which we caught in CI rather than by trial and error
later).

### What landed

1. **PR #5 — JSONL decision log.** Per-day file
   `logs/decisions/YYYY-MM-DD.jsonl`, one row per market per pass, both
   Trade and NoTrade. Owns a small `DecisionLogger` with a mutex around
   file appends so concurrent passes can't interleave bytes mid-line.
   Sparse-on-purpose for NoTrade rows: `NoOrderbook` carries no
   quote-derived fields, `SpreadTooWide` has spread but no edge, etc. —
   we record what the strategy actually computed rather than fabricating
   values.
2. **PR #6 — CI workflow.** `.github/workflows/ci.yml` runs `fmt --check
   / clippy -D warnings / build --locked / test --locked` on every PR
   to `main` and every push to `main`. Required two prep commits: a
   workspace-wide `cargo fmt --all` sweep (the existing tree had
   accumulated drift) and a clippy fix that boxed `EvBreakdown` inside
   `Decision::Trade` (`clippy::large_enum_variant`). The boxing change
   later forced a one-line fix in PR #5's test helper after the rebase
   chain settled — tracked through and merged cleanly.
3. **PR #7 — IEM CLI fetcher.** `weather-forecast::cli` pulls the daily
   high/low/precip per (station, date) from Iowa State's parsed JSON
   view of every NWS CLI product. Verified the response shape live
   before writing the parser. Documented IEM's footgun: omitting
   `?year=...&month=...` returns *2019* data by default (the earliest
   year IEM has parsed), not the current year. `fetch_recent` walks
   calendar months explicitly to avoid this.
4. **PR #8 — RiskManager.** Replaced the 5-line stub with a real layer:
   per-market position cap (`max_position_size_usd / price` floor),
   per-pass total-exposure cap, concurrent-positions cap. State is
   per-strategy-pass; `reset_for_pass()` clears between passes since
   dry-run has no fill confirmations to persist. Decision shape:
   `Approve(Signal) | Adjusted(Signal, AdjustReason) | Reject(RejectReason)`.
   Surfaced a flaky test in PR #5's decision log along the way:
   `tokio::fs::File` doesn't flush on drop, and under parallel cargo
   test the buffered write race occasionally lost. One-line fix
   (`file.flush().await?`) bundled into this PR.
5. **PR #9 — Kalshi RSA auth + kill-switched executor.** Real
   `KalshiSigner` (PKCS#8 + PKCS#1 PEM loaders), exact spec
   concatenation `(timestamp_ms ‖ method ‖ path)`, base64 PSS-SHA256
   signature, KALSHI-ACCESS-{KEY,TIMESTAMP,SIGNATURE} headers.
   `OrderRequest::from_signal(&Signal)` builds a buy-side post-only
   limit. `KalshiOrderClient::place_order` builds → signs → logs → if
   `never_send=true`, returns `Ok(None)` before touching the network.
   `never_send` is hard-coded `true` and only flippable by a code-level
   `client.allow_real_sends()` call — by design, so a stray env var
   can't accidentally turn the bot live.
6. **PR #10 — GEFS ensemble fetcher.** `weather-forecast::gefs` pulls
   30 perturbed GFS-0.5° members from Open-Meteo's `/v1/ensemble`,
   verified live before the parser was written. Aggregates per-member
   daily extremes inside the same `daily_high_window_utc` /
   `daily_low_window_utc` bucket pricing already uses, returning
   `(μ, σ, n_members)`. Wiring into pricing was deferred to a separate
   PR — substrate first.
7. **PR #11 — Backtest replay + DecisionRecord schema bump.** New
   `weather-backtest` crate. Reads JSONL via a deserialize-only
   `DecisionRow`, joins to IEM CLI, computes hit rate / Brier / log
   loss / mean signed bias / 10-bucket calibration. The schema bump:
   `DecisionRecord` gained `stat`, `direction`, `strike_temperature_f`,
   `resolution_date` so the join is possible in the first place.
   Non-additive (no `#[serde(default)]`); green-field is fine because
   no production JSONL exists yet.

### Concept learned: when CI lands mid-stack, dependency rebases get loud

PR #6 was cut from `main` *before* PR #5 merged. After #5 landed, `main`
grew a `decision_log` test that constructed `Decision::Trade(sig, ev)`
un-boxed — but #6's whole purpose was to box `EvBreakdown` to fix
`large_enum_variant`. When CI rebased #6 onto current `main`, the
un-boxed test broke compile. Fixed in one line. The lesson: when a
foundational PR is in flight, every other open branch is implicitly
held until the foundational PR's API change ripples through. The fix
was small here, but the pattern is worth flagging — adopt it as a
rebase discipline for the next round of stacked PRs.

### State after sweep 1
- At that point: 11-crate workspace; 89 unit tests + 3 ignored live-network integration
  tests (CLI, GEFS, Kalshi-auth).
- CI green on every PR through #11.
- Bot still in dry-run; static σ table still in use; executor not yet
  called from the strategy loop. JSONL logging is on by default.

---

## 2026-05-04 — Sweep 2: post-Perplexity hardening (PRs #12–#16)

A second arc, prompted by cross-referencing what we'd built against an
external research thread on Kalshi weather strategy (Perplexity output,
captured in the conversation log). Two themes: **risk gates the original
sweep was missing**, and **wiring the GEFS substrate into pricing for
real**.

### Decision: take Plan B over Plan A on historical ensemble σ

The research thread asked for a "historical backtest with GEFS-derived
σ". When we probed Open-Meteo, its `historical-forecast-api` returns
`404 Not Found` for the `/v1/ensemble` path — the archive only has
deterministic forecasts. Two paths to historical ensemble σ:

- **Plan A**: AWS `noaa-gefs-pds` GRIB2 archive. Heavy GRIB2 parsing
  in Rust. Real engineering, multi-day PR.
- **Plan B**: skip historical-σ-with-GEFS, wire GEFS σ into the *live*
  pricing path. Forward dry-run accumulates JSONL we can backtest
  against later, with `sigma_source` stamped per row.

Plan B was the right call: it's where the production value is, and it
lets us answer the empirical question (does ensemble σ improve
calibration?) by *running the bot* rather than rebuilding history. The
research's "use today's GEFS σ uniformly across past dates" suggestion
would have produced meaningless calibration metrics — passed on it.

### What landed

1. **PR #12 — Price band + NWS-update lockout.** Two pre-go-live risk
   gates the strategy was missing.
   - Price band `[$0.20, $0.92]`: tail-bracket contracts carry
     asymmetric blow-up risk (per the Northlake Labs 0-32 postmortem
     the research cited). New `NoTradeReason::PriceOutOfBand` fires
     after the edge gate but before `EvBelowGate`, so the JSONL shows
     the rejection was a risk decision, not a missing-edge one.
   - 30-min post-NWS-update lockout: arb bots reprice within seconds of
     each NWS issue. `weather-types::Forecast` grew an
     `Option<DateTime<Utc>> generated_at`; `weather-forecast` parses
     `properties.generatedAt` (with `updateTime` fallback);
     `main.rs::nws_lockout_decision` short-circuits trades inside the
     window with `NoTradeReason::ForecastTooFresh`.
2. **PR #13 — Open-Meteo historical-forecast + Phase A substrate.**
   `weather-forecast::historical_forecast` pulls archived deterministic
   GFS for a (city, date range) — verified live shape before coding.
   `weather-backtest::historical` walks a strike grid and synthesizes
   `JoinedDecision`s the existing `metrics()` aggregates over.
   Same-day horizon only — Open-Meteo's archive is keyed on valid date,
   not issue time. Multi-horizon backtest is out of scope until we have
   AWS noaa-gefs-pds or similar.
3. **PR #14 — `calibrate` binary.** End-to-end CLI: `cargo run -p
   weather-backtest --bin calibrate -- --city NY --start 2025-04-01
   --end 2025-09-30 --high-strikes 60,65,70,75,80,85,90,95`. Pulls
   archived GFS + IEM CLI, walks the strike grid, prints headline
   metrics + 10-bucket calibration table + Phase-A acceptance-criteria
   reminder so the operator doesn't have to cross-reference ROADMAP.md
   to interpret the numbers. Static σ baseline; GEFS σ wiring was held
   for the next PR.
4. **PR #15 — Wire GEFS ensemble σ into the live pricing path.** The
   payoff of PR #10's substrate. New
   `weather-pricing::price_market_with_sigma(threshold, forecast,
   sigma_override)` accepts a runtime σ; `price_market` is a thin
   wrapper that defaults to `None` (= `sigma_for_horizon`). Bot's main
   loop grew an `EnsembleCache` parallel to `ForecastCache`,
   refresh-on-demand on a longer cadence (GEFS publishes every ~6h),
   and `resolve_gefs_sigma` for the per-(city, date, stat) σ derivation
   with defensive fallbacks (`n_members < 5` or `σ < 0.25°F` falls back
   to static).
5. **PR #16 — `sigma_source` stamped on every priced decision.**
   `ModelPricing.sigma_source: &'static str` ("gefs_ensemble" or
   "static") populated by `price_market_with_sigma`. Threaded through
   `DecisionRecord` (JSONL writer) and `DecisionRow` (backtest reader).
   Synthetic decisions in `historical_calibration` always tag "static".
   Lets the backtester split metrics by source after a couple weeks of
   dry-run.

### Concept learned: instrument before measuring

PR #15 wired GEFS σ in but you couldn't tell from the JSONL whether a
given row used ensemble σ or fell back to static — `sigma_f` value
alone is a brittle inferer (any drift in `sigma_for_horizon` changes
the inference rule). PR #16 was the explicit-tag follow-up. Should
have been part of #15 in retrospect; instead each merged separately
because a clean schema bump felt better as its own commit. The
instrumentation cost is one string field per row; pay it before the
data accumulates.

### State after sweep 2
- 12 PRs total this session (#5 through #16).
- At that point: 11-crate workspace; **116 unit tests** + 3 ignored live-network tests.
- CI gates: `fmt --check`, `clippy -D warnings`, `build --locked`,
  `test --locked` enforced on every PR.
- Bot end-to-end runnable in dry-run with: NWS deterministic forecasts,
  GEFS ensemble σ (with static fallback), price band, NWS-update
  lockout, position/exposure/concurrent caps, per-day JSONL with
  `sigma_source` stamped, kill-switched RSA-PSS-SHA256 executor (not
  yet called from the strategy loop).
- All `feat/*` branches deleted; only `main` exists locally + remotely.

### What's next
Per the new "near-term" priority list at the bottom of `ROADMAP.md`:

1. **Run the bot in dry-run for ~2 weeks.** No code change.
   `cargo run -p weather-bot` accumulates `logs/decisions/*.jsonl`.
   Don't add new sources or strategies on top of an unmeasured baseline.
2. **Reconcile our station table against settled Kalshi markets.**
   Phase 1's open item — pull the last 30 days of resolved KXHIGH/KXLOW
   from Kalshi, diff against IEM CLI.
3. **Wire executor into strategy loop.** Behind `mode = "live"` and the
   existing `never_send` kill-switch.

Item 1 still gates everything else: without measured calibration we'd be
adding code on top of an unverified model.

---

## 2026-05-05 — Replay diagnostics + NWS timestamp parser fix

The first live dry-run surfaced two operational issues quickly:

1. NWS forecast payloads can include both `properties.generatedAt` and
   `properties.updateTime`. Our parser had modeled `updateTime` as a serde
   alias for `generatedAt`, so a payload carrying both fields failed with
   `duplicate field generatedAt`. The fix keeps them as separate optional
   wire fields, prefers `generatedAt`, and falls back to `updateTime`.
2. The first replay output looked like "all losses" because it marked every
   repeated dry-run Trade row to the bid-side liquidation price inside the
   same candle. In dry-run, the bot has no order lifecycle or fill state, so
   the same opportunity can be emitted every pass. Immediate bid-side marks
   mostly measure spread + entry fee friction, not resolved strategy P&L.

### What landed

- **`weather-backtest --bin replay`** reads one or more decision JSONL files
  or directories, prints row counts by `sigma_source`, optionally joins IEM
  CLI outcomes for resolved calibration metrics, and optionally fetches
  Kalshi candles for an immediate spread/fee mark.
- Replay **dedupes repeated Trade emissions** into unique opportunities using
  `(ticker, side, limit_price, contracts)` before candle marking, while still
  reporting the raw Trade row count and how many rows were collapsed.
- The candle section is now explicitly labelled
  **"Immediate candle spread/fee mark"** and prints a warning that it is not
  realised strategy P&L.
- `weather-scanner::candles` is now used by replay for recent Kalshi
  candlesticks. Older long-window backtests still need Kalshi's historical
  candle endpoint or an archive path.

### State after this fix

- Dry-run plumbing verified live: market scan succeeded, NWS forecasts parsed,
  GEFS σ rows were logged, `forecast_fetch_failed=0`, and per-pass summaries
  showed priced markets and explicit no-trade reasons.
- Workspace checks green: `cargo fmt --all`,
  `cargo clippy --workspace --all-targets -- -D warnings`, and
  `cargo test --workspace --locked`.
- Current replay is good for operational sanity and resolved calibration once
  CLI reports are available. It is not yet a fill-aware P&L backtester.
