//! Risk manager: applies position-size, total-exposure, and concurrent-
//! position caps to strategy signals before the executor sees them.
//!
//! The strategy emits a contract count as a Kelly hint, ignoring real
//! dollar caps. The risk layer converts that hint into a count that
//! actually fits the configured ceilings and may down-clip or reject.
//!
//! State is kept *in process for the current strategy pass*. We're still
//! dry-run, so there's no real "open positions" book yet — running tallies
//! reset every pass via `reset_for_pass`. When live trading lands, this
//! struct is the right place to plug in a persisted positions view from
//! `/portfolio/positions`.
//!
//! v1 scope (matches Phase 6 of the roadmap, items 1/2/5):
//!   - per-market position cap (`max_position_size_usd`)
//!   - per-pass total exposure cap (`max_total_exposure_usd`)
//!   - concurrent positions cap (`max_concurrent_positions`)
//!
//! Out of scope here, will land later: per-market cooldown, per-(city,date)
//! correlated-exposure cap, bankroll-fraction Kelly, kill switch.

use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use tracing::debug;

use weather_config::RiskConfig;
use weather_types::Signal;

/// What the risk layer did to a `Signal`.
#[derive(Debug, Clone)]
pub enum RiskDecision {
    /// Original signal cleared every cap unchanged.
    Approve(Signal),
    /// Signal was clipped to a smaller size to fit the caps. Reason
    /// indicates which cap binding'd.
    Adjusted(Signal, AdjustReason),
    /// Signal can't be placed at any size — caps fully exhausted.
    Reject(RejectReason),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdjustReason {
    /// Per-market cap reduced the contract count.
    PositionSizeClipped,
    /// Total exposure cap reduced (further) the contract count.
    TotalExposureClipped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RejectReason {
    /// At/over `max_concurrent_positions` already; refuse the new signal.
    ConcurrentPositionsCapped,
    /// Caps would force `contracts = 0` (e.g., total exposure already at
    /// the ceiling, or per-market cap is below the price of one contract).
    NoBudgetRemaining,
}

pub struct RiskManager {
    cfg: RiskConfig,
    /// Running USD exposure committed *this pass*. Resets via
    /// `reset_for_pass`.
    pass_exposure_usd: Decimal,
    /// Running count of approved-or-adjusted signals *this pass*.
    pass_position_count: usize,
}

impl RiskManager {
    pub fn new(cfg: RiskConfig) -> Self {
        Self {
            cfg,
            pass_exposure_usd: Decimal::ZERO,
            pass_position_count: 0,
        }
    }

    /// Drop the per-pass running tallies. Call at the start of each
    /// strategy pass — dry-run has no fill confirmations, so each pass
    /// starts fresh.
    pub fn reset_for_pass(&mut self) {
        self.pass_exposure_usd = Decimal::ZERO;
        self.pass_position_count = 0;
    }

    pub fn pass_exposure_usd(&self) -> Decimal {
        self.pass_exposure_usd
    }

    pub fn pass_position_count(&self) -> usize {
        self.pass_position_count
    }

    /// Apply caps to a `Signal`. Returns the (possibly clipped) signal,
    /// or a reject. Mutates the running tallies on Approve/Adjusted.
    pub fn evaluate(&mut self, signal: Signal) -> RiskDecision {
        if self.pass_position_count >= self.cfg.max_concurrent_positions {
            return RiskDecision::Reject(RejectReason::ConcurrentPositionsCapped);
        }

        let original_contracts = signal.contracts;
        let price = signal.limit_price;

        // Per-market cap. Floor of `cap / price`, clamped to the strategy's
        // requested size. price > 0 by strategy invariant; defensively guard.
        if price <= Decimal::ZERO {
            return RiskDecision::Reject(RejectReason::NoBudgetRemaining);
        }
        let per_market_max = (self.cfg.max_position_size_usd / price)
            .floor()
            .to_u32()
            .unwrap_or(0);
        let mut contracts = original_contracts.min(per_market_max);
        let mut adjustment: Option<AdjustReason> = None;
        if contracts < original_contracts {
            adjustment = Some(AdjustReason::PositionSizeClipped);
        }

        // Total-exposure cap. Re-clip if needed.
        let remaining_budget = self.cfg.max_total_exposure_usd - self.pass_exposure_usd;
        if remaining_budget <= Decimal::ZERO {
            return RiskDecision::Reject(RejectReason::NoBudgetRemaining);
        }
        let exposure_max = (remaining_budget / price).floor().to_u32().unwrap_or(0);
        if contracts > exposure_max {
            contracts = exposure_max;
            adjustment = Some(AdjustReason::TotalExposureClipped);
        }

        if contracts == 0 {
            return RiskDecision::Reject(RejectReason::NoBudgetRemaining);
        }

        let mut clipped = signal;
        clipped.contracts = contracts;

        let added_exposure = price * Decimal::from(contracts);
        self.pass_exposure_usd += added_exposure;
        self.pass_position_count += 1;
        debug!(
            ticker = %clipped.market_ticker,
            contracts,
            original_contracts,
            added_exposure = %added_exposure,
            pass_exposure = %self.pass_exposure_usd,
            pass_count = self.pass_position_count,
            "risk: evaluated"
        );

        match adjustment {
            None => RiskDecision::Approve(clipped),
            Some(reason) => RiskDecision::Adjusted(clipped, reason),
        }
    }
}

/// Dollars of exposure a given (price, contracts) pair represents.
/// Convenience for tests and the strategy log.
pub fn exposure_usd(price: Decimal, contracts: u32) -> Decimal {
    price * Decimal::from(contracts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use rust_decimal_macros::dec;
    use uuid::Uuid;
    use weather_types::Side;

    fn cfg(per_market: Decimal, total: Decimal, max_concurrent: usize) -> RiskConfig {
        RiskConfig {
            max_position_size_usd: per_market,
            max_total_exposure_usd: total,
            per_market_cooldown_secs: 60,
            max_concurrent_positions: max_concurrent,
        }
    }

    fn signal(price: Decimal, contracts: u32) -> Signal {
        Signal {
            id: Uuid::nil(),
            market_ticker: "KXHIGHNY-26JUL04-T75".into(),
            side: Side::Yes,
            limit_price: price,
            contracts,
            model_yes_probability: dec!(0.65),
            edge: dec!(0.15),
            generated_at: Utc::now(),
        }
    }

    #[test]
    fn under_caps_approves_unchanged() {
        let mut rm = RiskManager::new(cfg(dec!(5), dec!(50), 10));
        let s = signal(dec!(0.50), 5); // 5 * 0.50 = $2.50 ≤ $5
        let d = rm.evaluate(s);
        match d {
            RiskDecision::Approve(approved) => {
                assert_eq!(approved.contracts, 5);
                assert_eq!(approved.limit_price, dec!(0.50));
            }
            other => panic!("expected Approve, got {:?}", other),
        }
        assert_eq!(rm.pass_exposure_usd, dec!(2.50));
        assert_eq!(rm.pass_position_count, 1);
    }

    #[test]
    fn per_market_cap_clips_contracts() {
        // $5 cap / $0.50 = 10 max; strategy asked for 50.
        let mut rm = RiskManager::new(cfg(dec!(5), dec!(50), 10));
        let s = signal(dec!(0.50), 50);
        match rm.evaluate(s) {
            RiskDecision::Adjusted(adjusted, AdjustReason::PositionSizeClipped) => {
                assert_eq!(adjusted.contracts, 10);
            }
            other => panic!("expected PositionSizeClipped, got {:?}", other),
        }
        assert_eq!(rm.pass_exposure_usd, dec!(5));
    }

    #[test]
    fn total_exposure_cap_takes_over_when_per_market_already_fits() {
        // Per-market is generous ($100); total is tight ($3).
        let mut rm = RiskManager::new(cfg(dec!(100), dec!(3), 10));
        let s = signal(dec!(0.50), 50); // strategy wants 50
        match rm.evaluate(s) {
            RiskDecision::Adjusted(adjusted, AdjustReason::TotalExposureClipped) => {
                // $3 / $0.50 = 6 contracts max.
                assert_eq!(adjusted.contracts, 6);
            }
            other => panic!("expected TotalExposureClipped, got {:?}", other),
        }
    }

    #[test]
    fn second_signal_eats_remaining_total_budget() {
        let mut rm = RiskManager::new(cfg(dec!(100), dec!(5), 10));
        // First fills $2.
        let _ = rm.evaluate(signal(dec!(0.50), 4));
        assert_eq!(rm.pass_exposure_usd, dec!(2));

        // Remaining budget = $3; per-market is generous; ask for 100 @ $0.50.
        let d = rm.evaluate(signal(dec!(0.50), 100));
        match d {
            RiskDecision::Adjusted(s, AdjustReason::TotalExposureClipped) => {
                assert_eq!(s.contracts, 6); // $3 / $0.50
            }
            other => panic!("expected TotalExposureClipped, got {:?}", other),
        }
        assert_eq!(rm.pass_exposure_usd, dec!(5));
    }

    #[test]
    fn no_remaining_budget_rejects() {
        let mut rm = RiskManager::new(cfg(dec!(100), dec!(2), 10));
        let _ = rm.evaluate(signal(dec!(1.0), 2)); // exactly fills
        let d = rm.evaluate(signal(dec!(0.50), 1));
        assert!(matches!(
            d,
            RiskDecision::Reject(RejectReason::NoBudgetRemaining)
        ));
    }

    #[test]
    fn concurrent_cap_blocks_extra_signals() {
        let mut rm = RiskManager::new(cfg(dec!(100), dec!(100), 2));
        let _ = rm.evaluate(signal(dec!(0.50), 1));
        let _ = rm.evaluate(signal(dec!(0.50), 1));
        let d = rm.evaluate(signal(dec!(0.50), 1));
        assert!(matches!(
            d,
            RiskDecision::Reject(RejectReason::ConcurrentPositionsCapped)
        ));
    }

    #[test]
    fn reset_clears_pass_state() {
        let mut rm = RiskManager::new(cfg(dec!(100), dec!(2), 10));
        let _ = rm.evaluate(signal(dec!(1.0), 2));
        assert_eq!(rm.pass_exposure_usd, dec!(2));

        rm.reset_for_pass();
        assert_eq!(rm.pass_exposure_usd, dec!(0));
        assert_eq!(rm.pass_position_count, 0);

        // Same signal now passes again post-reset.
        let d = rm.evaluate(signal(dec!(0.50), 2));
        assert!(matches!(d, RiskDecision::Approve(_)));
    }

    #[test]
    fn per_market_cap_smaller_than_one_contract_rejects() {
        // $0.10 cap, $0.50 price → 0 max contracts.
        let mut rm = RiskManager::new(cfg(dec!(0.10), dec!(100), 10));
        let d = rm.evaluate(signal(dec!(0.50), 1));
        assert!(matches!(
            d,
            RiskDecision::Reject(RejectReason::NoBudgetRemaining)
        ));
    }

    #[test]
    fn exposure_usd_helper_matches_internal_tally() {
        assert_eq!(exposure_usd(dec!(0.50), 7), dec!(3.5));
    }
}
