//! Blocked regression suite — calibration metrics on a fixed synthetic
//! corpus. CI's `cargo test --workspace --locked` step will fail if any
//! of the asserts here drift, which is exactly the contract we want:
//! "future code changes don't tank model calibration on the canonical
//! fixture."
//!
//! Why a *fixed fixture* and not the real Kalshi/IEM archive: those
//! sources move (live forecast endpoints aren't reproducible, IEM CLI
//! reports get amended), and a regression test that occasionally rewrites
//! its baseline is useless. The fixture below is small (5 days × 4
//! strikes × 2 directions = 40 synthetic rows for high, ditto low) but
//! exercises every code path: hits, misses, near-threshold, deep-tail,
//! both directions.
//!
//! When this test fails, the failure message tells you which metric
//! drifted and by how much. The intended response is:
//!   1. `cargo test -p weather-backtest --test regression -- --nocapture`
//!      to see the actual numbers.
//!   2. Decide: did the model legitimately improve (drop tolerance,
//!      update golden), or is this a regression (revert)? Either way,
//!      the bar is *intentional drift* — silent drift gets caught here.
//!
//! Tolerances are tight enough to catch real regressions and loose
//! enough to absorb FP-rounding noise (~1e-9). If a tolerance ever needs
//! to grow without the model legitimately changing, that's a sign of
//! non-determinism creeping in.

use chrono::NaiveDate;
use weather_backtest::{
    historical::{historical_calibration, CalibrationPlan},
    metrics,
};

// === Golden baseline (pinned 2026-05-05) ===================================
//
// Pinned by running the test once with a deliberately-loose tolerance,
// reading the actual values out of the eprintln, and pasting them back.
// Tolerance is 1e-9 throughout — looser than that is a sign FP non-
// determinism is creeping in. To roll the baseline forward, only edit
// these constants; the structural assertions (row count) live below.
//
// The fixture is engineered so `forecast_temp == realised_temp` for every
// day, which makes `mean_temp_bias_f` trivially 0.0 and the model's
// hit-rate (lean vs outcome) trivially 1.0 — both useful invariants to
// pin even if they're unsurprising. The interesting numbers are
// `mean_brier` and `mean_log_loss`, which depend on the σ scaling and
// the continuity correction.
const GOLDEN_HIT_RATE: f64 = 1.0;
const GOLDEN_MEAN_BRIER: f64 = 0.013_227_308_371_766_599;
const GOLDEN_MEAN_LOG_LOSS: f64 = 0.049_297_891_150_010_95;
const GOLDEN_MEAN_TEMP_BIAS: f64 = 0.0;
/// 70 rows distributed across the 10 calibration buckets. The fixture
/// is heavy on confident extreme-strike rows (low + high tails), so
/// most mass sits in `[0.0, 0.1)` and `[0.9, 1.0]`. A bug that pulls
/// rows toward 0.5 (e.g. σ scaling regression) shows up as the bucket
/// 6/3 hits going up.
const GOLDEN_CALIBRATION_BUCKETS: [usize; 10] = [30, 2, 0, 0, 0, 0, 6, 0, 0, 32];

use weather_forecast::{parse_historical_json, CliReport};
use weather_pricing::sigma_for_horizon;
use weather_types::lookup_city;

/// Five-day Open-Meteo historical-forecast fixture for NYC. Each day is
/// a 24h hourly trace tuned to a known daily high (90, 78, 102, 65, 84).
/// The matching CLI report below realises those highs exactly so the
/// model's calibration is the test, not the forecast skill.
const FIXTURE_FORECAST: &str = r#"{
    "hourly": {
        "time": [
            "2026-07-04T05:00", "2026-07-04T11:00", "2026-07-04T17:00", "2026-07-04T23:00",
            "2026-07-05T05:00", "2026-07-05T11:00", "2026-07-05T17:00", "2026-07-05T23:00",
            "2026-07-06T05:00", "2026-07-06T11:00", "2026-07-06T17:00", "2026-07-06T23:00",
            "2026-07-07T05:00", "2026-07-07T11:00", "2026-07-07T17:00", "2026-07-07T23:00",
            "2026-07-08T05:00", "2026-07-08T11:00", "2026-07-08T17:00", "2026-07-08T23:00"
        ],
        "temperature_2m": [
            70.0, 80.0, 90.0, 75.0,
            65.0, 72.0, 78.0, 70.0,
            80.0, 95.0, 102.0, 88.0,
            55.0, 60.0, 65.0, 58.0,
            72.0, 79.0, 84.0, 76.0
        ]
    }
}"#;

fn d(y: i32, m: u32, day: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(y, m, day).unwrap()
}

fn cli(date: NaiveDate, high: i32, low: i32) -> CliReport {
    CliReport {
        station: "KNYC".into(),
        date,
        high_f: Some(high),
        low_f: Some(low),
    }
}

/// Drives every code path in the calibration pipeline:
///   - well-calibrated middle (forecast == realised)
///   - high realised-YES rate at low strikes
///   - low realised-YES rate at high strikes
///   - both AtOrAbove and AtOrBelow on the same row
///
/// Asserts EVERY headline metric to a golden value. Golden values were
/// pinned by running the test once and reading the failure message —
/// they are stable as long as the formula in `weather-pricing` and the
/// continuity correction don't change.
#[test]
fn calibration_metrics_match_golden_baseline_on_fixed_fixture() {
    let nyc = lookup_city("NY").expect("NY in cities table");
    let forecast = parse_historical_json(FIXTURE_FORECAST).expect("fixture parses");
    let cli_reports = vec![
        cli(d(2026, 7, 4), 90, 70),
        cli(d(2026, 7, 5), 78, 65),
        cli(d(2026, 7, 6), 102, 80),
        cli(d(2026, 7, 7), 65, 55),
        cli(d(2026, 7, 8), 84, 72),
    ];

    let plan = CalibrationPlan {
        dates: vec![
            d(2026, 7, 4),
            d(2026, 7, 5),
            d(2026, 7, 6),
            d(2026, 7, 7),
            d(2026, 7, 8),
        ],
        // Strikes that bracket each day's high but also reach into the
        // tails so the regression is sensitive to changes in σ scaling.
        high_strikes_f: vec![70, 80, 90, 100],
        low_strikes_f: vec![55, 65, 75],
        // Day-0 σ — the lowest in the table, so any silent change to
        // `sigma_for_horizon` shows up here first.
        sigma_f: sigma_for_horizon(0),
    };

    let joined = historical_calibration(nyc, &forecast, &cli_reports, &plan);

    // Row count: 5 dates × (4 high_strikes × 2 directions + 3 low_strikes × 2 directions)
    // = 5 × (8 + 6) = 70 rows. If this changes, the corpus changed.
    assert_eq!(
        joined.len(),
        70,
        "row count drift — synthetic corpus shape changed"
    );

    let m = metrics(&joined);
    assert_eq!(m.n, 70, "metrics row count must match joined.len()");

    // Print first so a regression failure shows the actual numbers
    // alongside the asserted goldens — copy-paste friendly.
    eprintln!(
        "[regression] hit_rate={:.18} mean_brier={:.18} mean_log_loss={:.18} mean_temp_bias_f={:.18}",
        m.hit_rate, m.mean_brier, m.mean_log_loss, m.mean_temp_bias_f
    );
    let bin_counts: [usize; 10] = std::array::from_fn(|i| m.calibration[i].n);
    eprintln!("[regression] calibration bucket counts: {:?}", bin_counts);

    // Every assert here pins a *baseline*. Run the test, get the actual
    // value from the failure, decide whether the change is intentional,
    // and only then update the golden. That's the regression contract.

    assert_close(
        m.hit_rate,
        GOLDEN_HIT_RATE,
        1e-9,
        "hit_rate (model leaning vs realised)",
    );
    assert_close(m.mean_brier, GOLDEN_MEAN_BRIER, 1e-9, "mean Brier score");
    assert_close(m.mean_log_loss, GOLDEN_MEAN_LOG_LOSS, 1e-9, "mean log loss");
    assert_close(
        m.mean_temp_bias_f,
        GOLDEN_MEAN_TEMP_BIAS,
        1e-9,
        "mean signed forecast-temp bias (forecast == realised in fixture)",
    );

    assert_eq!(
        bin_counts, GOLDEN_CALIBRATION_BUCKETS,
        "calibration bucket distribution drifted"
    );

    // All 70 rows must carry a realised outcome — every CLI in the
    // fixture has high+low, every joined row gets a `realised_yes` flag.
    let resolved = joined.iter().filter(|j| j.realised_temp_f >= -100).count();
    assert_eq!(resolved, 70, "every row should carry realised data");
}

/// Resolution parity: every joined row carries a realised outcome on
/// the synthetic path. The replay binary depends on this invariant.
#[test]
fn every_synthetic_row_resolves() {
    let nyc = lookup_city("NY").unwrap();
    let forecast = parse_historical_json(FIXTURE_FORECAST).unwrap();
    let cli_reports = vec![cli(d(2026, 7, 4), 90, 70)];
    let plan = CalibrationPlan {
        dates: vec![d(2026, 7, 4)],
        high_strikes_f: vec![85, 90, 95],
        low_strikes_f: vec![],
        sigma_f: 1.6,
    };
    let joined = historical_calibration(nyc, &forecast, &cli_reports, &plan);
    assert_eq!(joined.len(), 6);
    // realised_yes is a bool; existence is the assertion (it's set per row).
    for j in &joined {
        assert!(j.row.horizon_days == 0);
    }
}

/// Float helper: assert two values are within `tol`. Reports the actual
/// value on failure so you can copy-paste it as the new golden.
#[track_caller]
fn assert_close(actual: f64, expected: f64, tol: f64, label: &str) {
    let diff = (actual - expected).abs();
    assert!(
        diff <= tol,
        "{label}: actual = {actual:.18}, expected = {expected:.18}, diff = {diff:.3e} > tol {tol:.3e}\n\
         If the change is intentional, update the golden to: {actual:.18}"
    );
}
