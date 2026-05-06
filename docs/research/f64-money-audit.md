# `f64` audit — money paths

Goal: verify no `f64` is silently doing price math (USD per contract,
exposure, EV, fees). The roadmap's Phase 9 spot-check.

Status: **clean** as of this audit (post-bankroll-Kelly + cooldown
landing). No `f64` is on a money path.

## Methodology

```bash
grep -rn "f64" crates --include='*.rs' | grep -v 'tests'
```

Triaged every hit by what the value represents (units), not the type.

## Categories of `f64` use found

### 1. Geometry — lat/lon

Locations: `weather-types::Forecast { lat, lon }`, `weather-types::cities::CitySpec`,
`weather-forecast::*::fetch_*(lat, lon)`, `weather-bot::main` plumbing.

These are degrees of latitude/longitude. Not money. Promoted to `f64`
because the upstream APIs (NWS, Open-Meteo) want fractional degrees in
the URL.

### 2. Temperature math (°F)

Locations: `weather-pricing::yes_probability`,
`weather-forecast::gefs::{compute_extreme,daily_high_stats,daily_low_stats}`,
`weather-forecast::historical_forecast::compute_extreme_from_hourly`,
`weather-backtest::historical::price_via_normal_cdf`.

Inputs are integer °F (`i32`). Pricing converts to `f64` for the Normal
CDF, computes a probability in `[0, 1]`, then immediately upgrades to
`Decimal` via `Decimal::from_f64(yes_p).unwrap_or(Decimal::ZERO)` at the
crate boundary. The `f64` never crosses into a USD field.

### 3. σ (standard deviation, °F)

Locations: `weather-pricing::ModelPricing::sigma_f`, the full ensemble
math in `weather-forecast::gefs.rs`, the `sigma_for_horizon` table.

σ is a physics input, not a price. Kept as `f64` because it's
naturally one (sample standard deviation involves a square root, and we
don't need decimal-precision °F).

### 4. Probabilities and dimensionless ratios

Locations: `weather-backtest::lib::{Metrics, calibrate, summarize_pnl}`
fields like `hit_rate`, `mean_brier`, `mean_log_loss`, `win_rate`,
`mean_predicted_p`. The `LOG_LOSS_EPSILON: f64 = 1e-6` constant.

These are statistical summary metrics for replay/calibrate output, not
order sizes or costs. The authoritative `net_total: Decimal` lives
right alongside them; the `f64` mean is a display aid.

### 5. Calibration bin boundaries

Locations: `crates/weather-backtest/src/bin/{calibrate,replay}.rs`
(`(i as f64) * 0.1`).

UI bucket edges for the 10-bin calibration histogram — `0.0, 0.1, …,
1.0`. Not money.

### 6. JSONL parser intermediate (`serde_json::Number::as_f64`)

Locations: `weather-forecast::gefs::extract_temp_array`,
`weather-forecast::historical_forecast::parse_…`.

`serde_json::Number::as_f64()` returns `Option<f64>` for raw JSON
numbers — that's what Open-Meteo serves for temperatures. Same
"physics input" caveat as category 2.

## Verified safe paths (money stays `Decimal`)

The following are end-to-end `Decimal`:

- `KalshiMarket.{yes_bid, yes_ask, last_price, volume, open_interest}`
- `Signal.{limit_price, model_yes_probability, edge}` (`limit_price` is
  USD; `model_yes_probability` and `edge` are probabilities but kept as
  `Decimal` for arithmetic compatibility with prices)
- `EvBreakdown.{price, model_probability, market_implied_probability,
  spread, fee_estimate, safety_buffer, raw_edge, net_ev_per_contract,
  required_net_ev}`
- `RiskConfig.{max_position_size_usd, max_total_exposure_usd,
  bankroll_usd}` and the running `pass_exposure_usd`
- `KillSwitchConfig.max_drawdown_24h_usd`
- `StrategyConfig.{min_edge, kelly_fraction, safety_buffer,
  fee_multiplier, max_spread, min_price, max_price}`
- `weather-strategy::kelly_contracts` — bankroll, price, edge, stake all
  `Decimal`; only the final `to_u32()` exits the type
- `weather-executor::orders::price_to_cents` — `Decimal` → `u32` cents
- `weather-backtest::PnlEstimate.{entry_price, mark_price,
  fee_per_contract, gross_per_contract, net_per_contract, net_total}`

## Risk: f64 → Decimal precision loss in pricing crate

`Decimal::from_f64(yes_p).unwrap_or(Decimal::ZERO)` is the only place
an `f64` becomes a money-adjacent `Decimal`. `yes_p` is a probability
in `[0, 1]` produced by the Normal CDF. Worst-case rounding error:
~7.5e-8 (the Abramowitz–Stegun bound, documented on `normal_cdf`),
multiplied by `Decimal::from_f64`'s scale handling. This is well under
1¢ on a $1 contract — the EV gate's `safety_buffer = 0.01` absorbs
many orders of magnitude more.

Not a bug, but worth noting if a future refactor swaps in a
higher-precision Normal CDF: it should keep `Decimal::from_f64` as the
single conversion point and not introduce intermediate `f64` math
downstream.

## Conclusion

No action required. The codebase already obeys the rule "money is
`Decimal`, physics is `f64`, and the boundary between them is
`Decimal::from_f64` exactly once per pricing pass." This audit
documents the boundary so a future reviewer can spot a violation
quickly.
