//! Microbenchmarks for the pricing hot path.
//!
//! This is the inner loop of the strategy: for every market in the
//! tracked set, every pass, the bot calls `price_market_with_sigma` which
//! in turn calls `normal_cdf` twice (once for each tail). At 50 markets
//! per pass and a 5-second cadence, the loop sees ~36 000 pricing calls
//! per hour — small per-call cost, so we want to know the absolute floor
//! and notice if a refactor regresses it 5×.
//!
//! Ran via `cargo bench -p weather-pricing`. CI only verifies the bench
//! *compiles* (`cargo bench --no-run`); we don't run it in CI because
//! GitHub-hosted runners are too noisy to assert on absolute timings.
//! Use locally on a quiet machine to spot regressions.

use chrono::{DateTime, NaiveDate, Utc};
use criterion::{black_box, criterion_group, criterion_main, Criterion};
use weather_pricing::{normal_cdf, price_market_with_sigma, SIGMA_SOURCE_GEFS_ENSEMBLE};
use weather_types::{Forecast, ForecastPeriod, TempStat, ThresholdDirection, WeatherThreshold};

fn dt(s: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
}

fn nyc_july4_forecast(forecast_temp: i32) -> Forecast {
    Forecast {
        lat: 40.78,
        lon: -73.97,
        fetched_at: dt("2026-07-03T12:00:00Z"),
        generated_at: None,
        periods: vec![
            ForecastPeriod {
                name: "Saturday".into(),
                is_daytime: true,
                temperature_f: forecast_temp,
                precipitation_probability_pct: None,
                start_time: dt("2026-07-04T10:00:00Z"),
                end_time: dt("2026-07-04T22:00:00Z"),
                detailed_forecast: "".into(),
            },
            ForecastPeriod {
                name: "Saturday Night".into(),
                is_daytime: false,
                temperature_f: forecast_temp - 18,
                precipitation_probability_pct: None,
                start_time: dt("2026-07-04T22:00:00Z"),
                end_time: dt("2026-07-05T10:00:00Z"),
                detailed_forecast: "".into(),
            },
        ],
    }
}

fn high_threshold(strike: i32) -> WeatherThreshold {
    WeatherThreshold {
        city: "NY".into(),
        stat: TempStat::DailyHigh,
        direction: ThresholdDirection::AtOrAbove,
        temperature_f: strike,
        date: NaiveDate::from_ymd_opt(2026, 7, 4).unwrap(),
    }
}

/// Φ(z) — pure math, no allocations. The inner-most call inside pricing.
fn bench_normal_cdf(c: &mut Criterion) {
    let mut group = c.benchmark_group("normal_cdf");
    // A few representative z values: at the threshold (z=0), one σ out
    // (z=±1), and the deep tails (z=±3) where the polynomial branches
    // exercise different `exp` arguments.
    for z in [0.0_f64, 1.0, -1.0, 3.0, -3.0] {
        group.bench_with_input(format!("z={z}"), &z, |b, &z| {
            b.iter(|| normal_cdf(black_box(z)));
        });
    }
    group.finish();
}

/// `price_market_with_sigma` end-to-end: city lookup, settlement-window
/// computation, period selection, two CDF calls, and the final
/// `Decimal::from_f64`. This is what the strategy loop actually pays per
/// market.
fn bench_price_market(c: &mut Criterion) {
    let forecast = nyc_july4_forecast(82);
    let threshold = high_threshold(75);
    let mut group = c.benchmark_group("price_market_with_sigma");

    // Static σ path — the fallback that fires when GEFS isn't available.
    group.bench_function("static_sigma_fallback", |b| {
        b.iter(|| {
            price_market_with_sigma(black_box(&threshold), black_box(&forecast), black_box(None))
                .unwrap()
        });
    });

    // GEFS-override path — the "happy path" when ensemble σ is hot.
    group.bench_function("ensemble_sigma_override", |b| {
        b.iter(|| {
            price_market_with_sigma(
                black_box(&threshold),
                black_box(&forecast),
                black_box(Some((2.4, SIGMA_SOURCE_GEFS_ENSEMBLE))),
            )
            .unwrap()
        });
    });

    group.finish();
}

criterion_group!(benches, bench_normal_cdf, bench_price_market);
criterion_main!(benches);
