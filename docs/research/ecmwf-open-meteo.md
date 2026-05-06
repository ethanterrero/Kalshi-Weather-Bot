# ECMWF IFS Ensemble via Open-Meteo

Research notes for adding ECMWF as a second ensemble source alongside the existing
NOAA GEFS pull in `crates/weather-forecast/src/gefs.rs`. TL;DR: it's a one-line
`models=` swap with a different member count. Same endpoint, same response
schema, same units handling. The blender deserves a real decision; the parser
does not.

## Endpoint params

Same `https://ensemble-api.open-meteo.com/v1/ensemble` URL, same query knobs.

| Param              | GEFS (today)    | ECMWF (proposed)    | Notes                                                                |
| ------------------ | --------------- | ------------------- | -------------------------------------------------------------------- |
| `models=`          | `gfs05`         | `ecmwf_ifs025`      | Confirmed param name [1][2].                                         |
| Members            | 30 perturbed +1 | **50 perturbed +1** | ECMWF EPS publishes 51 total via Open-Meteo [1][2].                  |
| Forecast horizon   | up to 35 days   | up to 15 days       | We only ever ask for ~7, so this is non-binding [1].                 |
| Native step        | 3-hourly        | 3-hourly            | Open-Meteo upsamples to 1-hourly in the response either way [1][3].  |
| Run cadence        | every 6h        | every 6h            | Both refresh 4x/day at Open-Meteo [1].                               |
| Resolution         | 50 km           | 25 km               | Better near coasts/mountains [2] — relevant for SF, BOS.             |
| `temperature_unit` | `fahrenheit`    | `fahrenheit`        | Honored identically.                                                 |
| `timezone`         | `UTC`           | `UTC`               | Both return `utc_offset_seconds: 0`, `timezone: "GMT"` [3].          |

## Response shape

Verified live against `?latitude=40.78&longitude=-73.97&models=ecmwf_ifs025&hourly=temperature_2m&forecast_days=2&temperature_unit=fahrenheit&timezone=UTC` [3]:

```
hourly.time                       : Vec<String>   ("YYYY-MM-DDTHH:MM")
hourly.temperature_2m             : control run
hourly.temperature_2m_member01    : member 1
...
hourly.temperature_2m_member50    : member 50
```

That's **identical** to the GEFS shape we already parse. The control run lives
on the bare `temperature_2m` key (not `_control` — our existing parser already
handles this; the doc-comment on `gefs.rs:21` calling it the "control run" is
correct). The only delta is that the perturbed-member loop now needs to count
to 50 instead of 30.

Our existing parser already loops `for i in 1..=99u32 { ... break }` (gefs.rs:109),
so it picks up however many members are present. **Zero parser changes needed.**
The `daily_extreme_stats` math is also member-count-agnostic — `n` just gets
larger, which mechanically tightens σ a touch.

One caveat the docs don't make explicit: I assumed members are numbered
contiguously `01..=50` based on the GEFS convention and the live probe [3].
Before merging the ECMWF code path, run a manual `curl` and confirm member 50
exists and member 51 does not — the parser breaks on the first missing key, so
a non-contiguous numbering scheme would silently truncate.

## Implementation plan

Recommend: **add a `fetch_ecmwf_ifs025` method on the existing `GefsClient`,
not a new struct.** The "GEFS" name on the type is already a misnomer — it's
really an Open-Meteo ensemble client. Two reasonable refactors:

1. **Minimal** (do this first). Add `fetch_ecmwf_ifs025(lat, lon, days)` next
   to `fetch_gfs05`. Both call a private `fetch_model(model_id: &str, ...)`
   that builds the URL. Total diff ~30 lines + a live integration test mirroring
   `gefs_live_fetch_knyc_returns_thirty_members`, asserting `>= 40` members.
2. **Rename** (later, separate PR). Rename `gefs.rs` → `ensemble.rs`,
   `GefsClient` → `EnsembleClient`, `GefsError` → `EnsembleError`. Push an
   `enum EnsembleSource { Gfs05, EcmwfIfs025 }` into the public API so callers
   can ask for either by enum rather than method name. Don't bundle this with
   the ECMWF feature add — it's a search-and-replace churn PR that's easier to
   review on its own.

The `EnsembleForecast` / `EnsembleStat` structs are already source-agnostic, so
the daily-high/low math at the call site is unchanged. The pricing layer just
gets two `(μ, σ)` inputs instead of one.

## Blending

Two options worth implementing; pick one based on a backtest, don't agonize a
priori.

**Option A — pooled samples (simplest non-silly).** Concatenate the per-member
daily extremes from both ensembles into one `Vec<f64>` (length 31 + 51 = 82
for high/low) and recompute `(μ, σ)` over the union. This implicitly weights
sources by member count (ECMWF gets ~62% of the vote because it has more
members), which is roughly what we want — ECMWF's spread is more trusted in
the verification literature, and member count is a fine proxy for that.
Mechanically, this just means `daily_extreme_stats` accepts `&[&EnsembleForecast]`
instead of `&EnsembleForecast`.

**Option B — defensive σ (max).** Compute `(μ, σ)` per source, take the *larger*
σ, blend μ with inverse-variance or just an even average. Wider σ → closer to
50/50 priced markets → fewer trades but smaller losses on the trades you do
take. Useful if we're worried about ensemble underdispersion (a real,
documented problem with both GEFS and ECMWF EPS at sub-72h horizons), but
loses information when the ensembles agree.

Recommendation: ship Option A behind a feature flag, log Option B's σ and the
single-source σ side-by-side in the per-pass summary [bot/per-pass log line,
PR #18], and let two weeks of replays decide. The infrastructure for that
(replay + per-pass summaries) already exists.

## Open questions

- **Member numbering.** Confirmed 01..=50 via live probe [3], but only on one
  point. Worth a sanity-check `curl` for a second city before merging, and a
  defensive log in the parser if member count is unexpected (`!= 30 && != 50`).
- **Rate limits across both sources.** Open-Meteo's free tier is ~10k calls/day
  with weight `nDays/14 * nVars/10 * nLocations`. Crucially, ensemble member
  count **does not** appear in the documented weight formula [4][5], but the
  Substack post hints it might in practice [4]. Calling `models=gfs05,ecmwf_ifs025`
  in a single request is *probably* one billed call with merged response keys
  (e.g. `temperature_2m_member01_gfs05` vs `_ecmwf_ifs025`), but I haven't
  verified. Worth a `curl` test before assuming we can halve our request count.
- **Update timing skew.** Both refresh every 6h, but ECMWF runs (00/06/12/18Z)
  hit Open-Meteo with different lag than GEFS runs. We don't currently log
  the model run timestamp; we should, so we can tell which run a given σ
  came from when post-mortem-ing a bad trade.

## Sources

1. https://open-meteo.com/en/docs/ensemble-api — canonical endpoint docs.
2. https://openmeteo.substack.com/p/ecmwf-ifs-upgraded-to-025-resolution — the upgrade announcement; confirms 51 members, 15-day, 4x/day, `ecmwf_ifs025`.
3. Live probe: `GET https://ensemble-api.open-meteo.com/v1/ensemble?latitude=40.78&longitude=-73.97&hourly=temperature_2m&models=ecmwf_ifs025&temperature_unit=fahrenheit&forecast_days=2&timezone=UTC` (2026-05-05).
4. https://openmeteo.substack.com/p/api-subscriptions-for-commercial — call-weighting formula.
5. https://open-meteo.com/en/pricing — free tier limits (10k/day, 5k/hr, 600/min).
