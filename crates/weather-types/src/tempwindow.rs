//! Local-standard-time observation window for daily temperature settlement.
//!
//! From the Kalshi weather help page: "High temperature reporting uses local
//! standard time; during DST, the high-temp window is 1:00 AM through 12:59
//! AM local time the following day, not calendar midnight-to-midnight."
//!
//! That means the daily-high "day" Kalshi settles on, in standard-time clock
//! terms, is `[00:00, 23:59]` — but during DST the same NWS CLI report
//! corresponds to local clock-time `[01:00, 24:59]`. To compare a forecast
//! (which has wall-clock period boundaries) against a Kalshi market we need
//! the UTC instants that bound the standard-time day for that city + date.
//!
//! We model this by always working in **standard offset** for the city.
//! `2026-07-04` for Chicago (CST = UTC-6) → window = `[2026-07-04 00:00 CST,
//! 2026-07-05 00:00 CST)` = `[2026-07-04 06:00 UTC, 2026-07-05 06:00 UTC)`.
//! Even if Chicago is on CDT (UTC-5) on that calendar day, the window we
//! evaluate against forecast periods is the standard-time one Kalshi settles
//! on.

use chrono::{DateTime, Duration, NaiveDate, TimeZone, Utc};

use crate::cities::CitySpec;

/// Daily-HIGH settlement window in UTC for `date` at `city`. Returned as
/// `[start, end)` where the half-open semantics matter — a forecast period
/// starting exactly at `end` belongs to the *next* day.
pub fn daily_high_window_utc(city: &CitySpec, date: NaiveDate) -> (DateTime<Utc>, DateTime<Utc>) {
    standard_day_window_utc(city, date)
}

/// Daily-LOW window. Same convention as the high.
pub fn daily_low_window_utc(city: &CitySpec, date: NaiveDate) -> (DateTime<Utc>, DateTime<Utc>) {
    standard_day_window_utc(city, date)
}

fn standard_day_window_utc(
    city: &CitySpec,
    date: NaiveDate,
) -> (DateTime<Utc>, DateTime<Utc>) {
    let offset = city.standard_offset();
    let start_local = offset
        .from_local_datetime(&date.and_hms_opt(0, 0, 0).expect("00:00 always valid"))
        .single()
        .expect("standard offset is unambiguous");
    let start_utc = start_local.with_timezone(&Utc);
    let end_utc = start_utc + Duration::hours(24);
    (start_utc, end_utc)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cities::lookup_city;

    #[test]
    fn nyc_window_in_winter_aligns_with_est() {
        // 2026-01-15 is winter — local clock = standard time (UTC-5).
        // Standard-time day = [00:00 EST, 24:00 EST) = [05:00 UTC, 05:00 UTC next day).
        let nyc = lookup_city("NY").unwrap();
        let date = NaiveDate::from_ymd_opt(2026, 1, 15).unwrap();
        let (start, end) = daily_high_window_utc(nyc, date);
        assert_eq!(start.to_rfc3339(), "2026-01-15T05:00:00+00:00");
        assert_eq!(end.to_rfc3339(), "2026-01-16T05:00:00+00:00");
    }

    #[test]
    fn nyc_window_in_summer_still_uses_standard_offset() {
        // The whole point of this helper: even mid-summer, the Kalshi
        // settlement window is on standard offset (UTC-5 for NY), NOT EDT.
        // So the window stays at 05:00→05:00 UTC, not 04:00→04:00.
        let nyc = lookup_city("NY").unwrap();
        let date = NaiveDate::from_ymd_opt(2026, 7, 4).unwrap();
        let (start, end) = daily_high_window_utc(nyc, date);
        assert_eq!(start.to_rfc3339(), "2026-07-04T05:00:00+00:00");
        assert_eq!(end.to_rfc3339(), "2026-07-05T05:00:00+00:00");
    }

    #[test]
    fn chi_and_lax_have_correct_offsets() {
        let chi = lookup_city("CHI").unwrap();
        let date = NaiveDate::from_ymd_opt(2026, 7, 4).unwrap();
        let (start, _) = daily_high_window_utc(chi, date);
        assert_eq!(start.to_rfc3339(), "2026-07-04T06:00:00+00:00"); // CST = UTC-6

        let lax = lookup_city("LAX").unwrap();
        let (start, _) = daily_high_window_utc(lax, date);
        assert_eq!(start.to_rfc3339(), "2026-07-04T08:00:00+00:00"); // PST = UTC-8
    }

    #[test]
    fn window_is_exactly_24_hours() {
        let nyc = lookup_city("NY").unwrap();
        let date = NaiveDate::from_ymd_opt(2026, 3, 8).unwrap(); // DST start day in US
        let (start, end) = daily_high_window_utc(nyc, date);
        // Standard offset is fixed, so the UTC window is always 24h regardless
        // of the local DST transition that calendar day.
        assert_eq!((end - start).num_hours(), 24);
    }
}
