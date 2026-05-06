//! Per-source staleness check.
//!
//! A forecast source (NWS, GEFS) has an *expected refresh interval* — NWS
//! is hourly-ish, GEFS is ~6h. The bot's cache is gated on that interval:
//! if the cached value is older, we re-fetch. When the re-fetch fails
//! repeatedly, the cache *stays stale*, but the strategy loop's current
//! behavior is to skip the market without surfacing why — the operator
//! can't easily tell "NWS has been failing for an hour" vs "NWS just
//! blipped once."
//!
//! This module gives the loop a single classifier so the decision (and
//! its tag for logs / per-pass summary) stays consistent. Phase 2 of the
//! roadmap.

use std::time::Duration;

use chrono::{DateTime, Utc};

/// State of a source's cached value relative to its refresh contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceFreshness {
    /// Cache is empty or younger than the refresh interval. Either fine
    /// to use or about to be re-fetched on demand.
    Fresh,
    /// Cache exists, is older than `refresh_interval`, but younger than
    /// the staleness threshold. Refresh-on-demand will catch it; the
    /// caller doesn't need to scream.
    NeedsRefresh,
    /// Cache exists and is older than `staleness_multiplier *
    /// refresh_interval`. The source has effectively stopped delivering;
    /// caller should log a SourceStale warning and skip rather than feed
    /// stale data to the blend.
    Stale { age: Duration, threshold: Duration },
}

/// Classify a (last-success-time, refresh-interval) pair.
///
/// `staleness_multiplier` is how many refresh intervals must pass before
/// the cache is considered "stale" rather than just "needs refresh." 2 is
/// a reasonable default — one missed refresh is normal, two means the
/// source is actually broken.
///
/// `last_success` of `None` is treated as Fresh (cold-start case — there's
/// no stale data to worry about; the loop will fetch on demand).
pub fn classify(
    last_success: Option<DateTime<Utc>>,
    refresh_interval: Duration,
    staleness_multiplier: u32,
    now: DateTime<Utc>,
) -> SourceFreshness {
    let Some(ts) = last_success else {
        return SourceFreshness::Fresh;
    };
    let age = now
        .signed_duration_since(ts)
        .to_std()
        .unwrap_or(Duration::ZERO);
    if age <= refresh_interval {
        return SourceFreshness::Fresh;
    }
    let threshold = refresh_interval.saturating_mul(staleness_multiplier);
    if age > threshold {
        SourceFreshness::Stale { age, threshold }
    } else {
        SourceFreshness::NeedsRefresh
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// All tests use a fixed `now` and offset timestamps from it so the
    /// boundary cases are exact, not "approximately N seconds ago modulo
    /// scheduler jitter."
    fn fixed_now() -> DateTime<Utc> {
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2026, 5, 5, 12, 0, 0).unwrap()
    }

    fn ts_secs_before(now: DateTime<Utc>, secs: i64) -> DateTime<Utc> {
        now - chrono::Duration::seconds(secs)
    }

    #[test]
    fn missing_cache_is_fresh_not_stale() {
        assert_eq!(
            classify(None, Duration::from_secs(1800), 2, fixed_now()),
            SourceFreshness::Fresh
        );
    }

    #[test]
    fn cache_younger_than_interval_is_fresh() {
        let now = fixed_now();
        let ts = ts_secs_before(now, 60);
        assert_eq!(
            classify(Some(ts), Duration::from_secs(1800), 2, now),
            SourceFreshness::Fresh
        );
    }

    #[test]
    fn cache_at_interval_boundary_is_fresh() {
        let now = fixed_now();
        let ts = ts_secs_before(now, 1800);
        assert_eq!(
            classify(Some(ts), Duration::from_secs(1800), 2, now),
            SourceFreshness::Fresh
        );
    }

    #[test]
    fn cache_one_interval_past_is_needs_refresh() {
        let now = fixed_now();
        let ts = ts_secs_before(now, 2100); // 35 min ago, interval 30 min
        match classify(Some(ts), Duration::from_secs(1800), 2, now) {
            SourceFreshness::NeedsRefresh => {}
            other => panic!("expected NeedsRefresh, got {:?}", other),
        }
    }

    #[test]
    fn cache_past_staleness_threshold_is_stale() {
        let now = fixed_now();
        let ts = ts_secs_before(now, 5400); // 90 min ago, threshold 60 min
        match classify(Some(ts), Duration::from_secs(1800), 2, now) {
            SourceFreshness::Stale { age, threshold } => {
                assert_eq!(age, Duration::from_secs(5400));
                assert_eq!(threshold, Duration::from_secs(3600));
            }
            other => panic!("expected Stale, got {:?}", other),
        }
    }

    #[test]
    fn larger_multiplier_widens_the_stale_threshold() {
        let now = fixed_now();
        let ts = ts_secs_before(now, 5400);
        match classify(Some(ts), Duration::from_secs(1800), 4, now) {
            SourceFreshness::NeedsRefresh => {}
            other => panic!("expected NeedsRefresh with mult=4, got {:?}", other),
        }
    }

    #[test]
    fn future_timestamp_does_not_panic_or_classify_as_stale() {
        let now = fixed_now();
        let ts = now + chrono::Duration::seconds(60); // clock skew
        assert_eq!(
            classify(Some(ts), Duration::from_secs(1800), 2, now),
            SourceFreshness::Fresh
        );
    }
}
