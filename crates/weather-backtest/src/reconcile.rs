//! Settlement reconciliation: do our station-table + ticker-parser + IEM
//! CLI lookup agree with what Kalshi actually settled?
//!
//! For each settled Kalshi KXHIGH / KXLOW market in a recent window:
//!   1. Parse the ticker into a `WeatherThreshold` (city, stat, direction, strike, date).
//!   2. Look up the city's settlement station via `weather_types::cities`.
//!   3. Find the IEM CLI report for that `(station, date)` and pick the right extreme.
//!   4. Compute the YES outcome locally using `realised_yes_for`.
//!   5. Compare to Kalshi's `result` field.
//!
//! Drift is a station-mapping or ticker-parser bug — every metric we
//! print over the decision log is suspect until reconciliation is clean.
//! That's why this is Phase 1's last open item: it's a correctness gate
//! for everything downstream.

use chrono::NaiveDate;
use weather_forecast::CliReport;
use weather_scanner::{parse_weather_threshold, RawMarket};
use weather_types::{cities::lookup_city, TempStat, ThresholdDirection};

use crate::realised_yes_for;

/// How Kalshi settled the market.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KalshiOutcome {
    Yes,
    No,
}

/// Why a settled market was skipped without comparison.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipReason {
    /// Ticker doesn't parse as a supported weather threshold (between-bins,
    /// non-temp series, etc.).
    UnsupportedTicker,
    /// City code not in `weather_types::cities` — would silently mismap if
    /// we didn't refuse.
    UnknownCity(String),
    /// Kalshi response had no `result` field, or it wasn't `"yes"`/`"no"`.
    MissingOrUnparseableResult(Option<String>),
    /// No IEM CLI report joined to `(station, date)`.
    MissingCli { station: String, date: NaiveDate },
    /// CLI report exists but the relevant extreme is null.
    MissingTempInCli {
        station: String,
        date: NaiveDate,
        stat: TempStat,
    },
}

/// One reconciled comparison.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReconcileResult {
    pub ticker: String,
    pub city: String,
    pub station: String,
    pub resolution_date: NaiveDate,
    pub stat: TempStat,
    pub direction: ThresholdDirection,
    pub strike_temperature_f: i32,
    pub kalshi_result: KalshiOutcome,
    /// What our local pipeline computes from CLI + threshold.
    pub computed_yes: bool,
    /// The CLI extreme on the resolution date.
    pub realised_temp_f: i32,
    /// True iff Kalshi and we agree.
    pub agrees: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReconcileOutcome {
    Reconciled(ReconcileResult),
    Skipped { ticker: String, reason: SkipReason },
}

/// Reconcile one settled Kalshi market against the joined CLI slice.
pub fn reconcile_settled_market(raw: &RawMarket, cli: &[CliReport]) -> ReconcileOutcome {
    let Some(threshold) = parse_weather_threshold(&raw.ticker, &raw.strike_type) else {
        return ReconcileOutcome::Skipped {
            ticker: raw.ticker.clone(),
            reason: SkipReason::UnsupportedTicker,
        };
    };

    let Some(city) = lookup_city(&threshold.city) else {
        return ReconcileOutcome::Skipped {
            ticker: raw.ticker.clone(),
            reason: SkipReason::UnknownCity(threshold.city.clone()),
        };
    };

    let kalshi_result = match raw.result.as_deref().map(str::trim) {
        Some("yes") | Some("YES") | Some("Yes") => KalshiOutcome::Yes,
        Some("no") | Some("NO") | Some("No") => KalshiOutcome::No,
        other => {
            return ReconcileOutcome::Skipped {
                ticker: raw.ticker.clone(),
                reason: SkipReason::MissingOrUnparseableResult(other.map(String::from)),
            };
        }
    };

    let report = cli
        .iter()
        .find(|r| r.station == city.icao && r.date == threshold.date);
    let Some(report) = report else {
        return ReconcileOutcome::Skipped {
            ticker: raw.ticker.clone(),
            reason: SkipReason::MissingCli {
                station: city.icao.to_string(),
                date: threshold.date,
            },
        };
    };

    let realised_temp_f = match threshold.stat {
        TempStat::DailyHigh => report.high_f,
        TempStat::DailyLow => report.low_f,
    };
    let Some(realised_temp_f) = realised_temp_f else {
        return ReconcileOutcome::Skipped {
            ticker: raw.ticker.clone(),
            reason: SkipReason::MissingTempInCli {
                station: city.icao.to_string(),
                date: threshold.date,
                stat: threshold.stat,
            },
        };
    };

    let computed_yes = realised_yes_for(
        threshold.direction,
        threshold.temperature_f,
        realised_temp_f,
    );
    let kalshi_yes = matches!(kalshi_result, KalshiOutcome::Yes);

    ReconcileOutcome::Reconciled(ReconcileResult {
        ticker: raw.ticker.clone(),
        city: threshold.city,
        station: city.icao.to_string(),
        resolution_date: threshold.date,
        stat: threshold.stat,
        direction: threshold.direction,
        strike_temperature_f: threshold.temperature_f,
        kalshi_result,
        computed_yes,
        realised_temp_f,
        agrees: computed_yes == kalshi_yes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw_settled(ticker: &str, strike_type: &str, result: Option<&str>) -> RawMarket {
        // Minimal RawMarket — most fields aren't read by reconcile.
        let json = format!(
            r#"{{
                "ticker": "{ticker}",
                "event_ticker": "ignored",
                "title": "x",
                "yes_sub_title": "x",
                "close_time": "2026-05-11T04:59:00Z",
                "status": "finalized",
                "strike_type": "{strike_type}"
                {maybe_result}
            }}"#,
            maybe_result = match result {
                Some(r) => format!(r#", "result": "{r}""#),
                None => String::new(),
            }
        );
        serde_json::from_str(&json).expect("test fixture parses")
    }

    fn cli(station: &str, date: NaiveDate, high_f: Option<i32>, low_f: Option<i32>) -> CliReport {
        CliReport {
            station: station.to_string(),
            date,
            high_f,
            low_f,
        }
    }

    #[test]
    fn agrees_when_cli_temp_clears_strike_and_kalshi_says_yes() {
        // Ticker: "T64 + greater" → "high ≥ 65°F".
        // CLI: 80°F → ≥ 65, so YES.
        let raw = raw_settled("KXHIGHNY-26MAY10-T64", "greater", Some("yes"));
        let date = NaiveDate::from_ymd_opt(2026, 5, 10).unwrap();
        let cli_rows = vec![cli("KNYC", date, Some(80), Some(60))];

        let outcome = reconcile_settled_market(&raw, &cli_rows);
        match outcome {
            ReconcileOutcome::Reconciled(r) => {
                assert!(r.agrees);
                assert_eq!(r.station, "KNYC");
                assert_eq!(r.realised_temp_f, 80);
                assert_eq!(r.strike_temperature_f, 65);
                assert_eq!(r.kalshi_result, KalshiOutcome::Yes);
                assert!(r.computed_yes);
            }
            other => panic!("expected Reconciled, got {other:?}"),
        }
    }

    #[test]
    fn disagrees_when_kalshi_settles_yes_but_cli_says_no() {
        // Ticker: "T79 + greater" → "high ≥ 80°F".
        // CLI: 70°F → NOT ≥ 80, so our computed = NO. But Kalshi says YES.
        // This is the station-mismatch / parser-bug signal we want to see.
        let raw = raw_settled("KXHIGHNY-26MAY10-T79", "greater", Some("yes"));
        let date = NaiveDate::from_ymd_opt(2026, 5, 10).unwrap();
        let cli_rows = vec![cli("KNYC", date, Some(70), Some(60))];

        let outcome = reconcile_settled_market(&raw, &cli_rows);
        match outcome {
            ReconcileOutcome::Reconciled(r) => {
                assert!(!r.agrees);
                assert!(!r.computed_yes);
                assert_eq!(r.kalshi_result, KalshiOutcome::Yes);
            }
            other => panic!("expected Reconciled drift, got {other:?}"),
        }
    }

    #[test]
    fn skips_between_bin_tickers() {
        let raw = raw_settled("KXHIGHNY-26MAY10-B64.5", "between", Some("yes"));
        let date = NaiveDate::from_ymd_opt(2026, 5, 10).unwrap();
        let cli_rows = vec![cli("KNYC", date, Some(80), Some(60))];
        match reconcile_settled_market(&raw, &cli_rows) {
            ReconcileOutcome::Skipped {
                reason: SkipReason::UnsupportedTicker,
                ..
            } => {}
            other => panic!("expected UnsupportedTicker skip, got {other:?}"),
        }
    }

    #[test]
    fn skips_unknown_city() {
        let raw = raw_settled("KXHIGHZZZ-26MAY10-T64", "greater", Some("yes"));
        let date = NaiveDate::from_ymd_opt(2026, 5, 10).unwrap();
        match reconcile_settled_market(&raw, &[cli("KZZZ", date, Some(80), None)]) {
            ReconcileOutcome::Skipped {
                reason: SkipReason::UnknownCity(c),
                ..
            } => assert_eq!(c, "ZZZ"),
            other => panic!("expected UnknownCity skip, got {other:?}"),
        }
    }

    #[test]
    fn skips_missing_result_field() {
        let raw = raw_settled("KXHIGHNY-26MAY10-T64", "greater", None);
        let date = NaiveDate::from_ymd_opt(2026, 5, 10).unwrap();
        match reconcile_settled_market(&raw, &[cli("KNYC", date, Some(80), None)]) {
            ReconcileOutcome::Skipped {
                reason: SkipReason::MissingOrUnparseableResult(None),
                ..
            } => {}
            other => panic!("expected MissingOrUnparseableResult, got {other:?}"),
        }
    }

    #[test]
    fn skips_when_no_cli_for_station_date() {
        let raw = raw_settled("KXHIGHNY-26MAY10-T64", "greater", Some("yes"));
        // Empty CLI slice.
        match reconcile_settled_market(&raw, &[]) {
            ReconcileOutcome::Skipped {
                reason: SkipReason::MissingCli { station, date, .. },
                ..
            } => {
                assert_eq!(station, "KNYC");
                assert_eq!(date, NaiveDate::from_ymd_opt(2026, 5, 10).unwrap());
            }
            other => panic!("expected MissingCli, got {other:?}"),
        }
    }

    #[test]
    fn skips_when_relevant_extreme_missing_from_cli() {
        let raw = raw_settled("KXHIGHNY-26MAY10-T64", "greater", Some("yes"));
        let date = NaiveDate::from_ymd_opt(2026, 5, 10).unwrap();
        let cli_rows = vec![cli("KNYC", date, None, Some(60))]; // high_f missing
        match reconcile_settled_market(&raw, &cli_rows) {
            ReconcileOutcome::Skipped {
                reason: SkipReason::MissingTempInCli { stat, .. },
                ..
            } => assert_eq!(stat, TempStat::DailyHigh),
            other => panic!("expected MissingTempInCli, got {other:?}"),
        }
    }

    #[test]
    fn agrees_on_at_or_below_when_cli_low_clears_strike() {
        // KXLOW + "T35 + less" → low ≤ 34°F. CLI low = 28 → YES.
        let raw = raw_settled("KXLOWAUS-26JAN15-T35", "less", Some("yes"));
        let date = NaiveDate::from_ymd_opt(2026, 1, 15).unwrap();
        let cli_rows = vec![cli("KAUS", date, Some(40), Some(28))];
        match reconcile_settled_market(&raw, &cli_rows) {
            ReconcileOutcome::Reconciled(r) => {
                assert_eq!(r.station, "KAUS");
                assert_eq!(r.stat, TempStat::DailyLow);
                assert_eq!(r.direction, ThresholdDirection::AtOrBelow);
                assert_eq!(r.strike_temperature_f, 34);
                assert_eq!(r.realised_temp_f, 28);
                assert!(r.computed_yes);
                assert!(r.agrees);
            }
            other => panic!("expected Reconciled, got {other:?}"),
        }
    }
}
