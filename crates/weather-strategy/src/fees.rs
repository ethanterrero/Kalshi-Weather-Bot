//! Kalshi fee model.
//!
//! Source: https://help.kalshi.com/en/articles/13823805-fees and
//!         https://kalshi.com/fee-schedule
//!
//! The published formula is:
//!     fee_per_100 = round_up_to_cent( multiplier * 0.07 * 100 * p * (1 - p) )
//! For most weather markets multiplier = 1, giving the help-page-quoted
//! range $0.07–$1.75 per 100 contracts (peaks at p=0.5 → 1.75¢ per
//! contract). Some series have additional maker fees not relevant to v1
//! (we send takers — limit orders at the resting opposite-side ask).

use rust_decimal::Decimal;
use rust_decimal_macros::dec;

#[derive(Debug, Clone, Copy)]
pub struct FeeModel {
    /// Per-series fee multiplier. 1.0 for most weather markets per the help
    /// page; passed in here so unusual series can override.
    pub multiplier: Decimal,
}

impl Default for FeeModel {
    fn default() -> Self {
        Self {
            multiplier: Decimal::ONE,
        }
    }
}

/// Estimated fee per *single* contract at price `price` ($0.00–$1.00),
/// rounded up to the next cent (Kalshi rounds at the per-100 batch level
/// but for sizing purposes the per-contract conservative rounding is what
/// matters).
pub fn estimate_fee_per_contract(price: Decimal, fees: &FeeModel) -> Decimal {
    if price <= Decimal::ZERO || price >= Decimal::ONE {
        return Decimal::ZERO;
    }
    let raw = fees.multiplier * dec!(0.07) * price * (Decimal::ONE - price);
    // Round up to the next cent to avoid under-estimating fees in the EV
    // gate. Kalshi rounds the per-100 fee up; per-contract rounding is more
    // conservative.
    round_up_to_cent(raw)
}

fn round_up_to_cent(d: Decimal) -> Decimal {
    let cents = d * dec!(100);
    let ceil_cents = cents.ceil();
    ceil_cents / dec!(100)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fee_at_50_50_is_2_cents_per_contract() {
        // 0.07 * 0.5 * 0.5 = 0.0175 → round up = $0.02 per contract.
        // Per 100 contracts ≈ $1.75 — matches the help-page max.
        let f = estimate_fee_per_contract(dec!(0.50), &FeeModel::default());
        assert_eq!(f, dec!(0.02));
    }

    #[test]
    fn fee_at_extremes_is_one_cent() {
        // p=0.95 → 0.07 * 0.95 * 0.05 = 0.003325 → round up to $0.01.
        // Matches the lower end of the help-page range.
        let f = estimate_fee_per_contract(dec!(0.95), &FeeModel::default());
        assert_eq!(f, dec!(0.01));
    }

    #[test]
    fn fee_at_full_settlement_is_zero() {
        // p=0 or p=1 → no risk, no fee.
        assert_eq!(
            estimate_fee_per_contract(dec!(0), &FeeModel::default()),
            Decimal::ZERO
        );
        assert_eq!(
            estimate_fee_per_contract(dec!(1), &FeeModel::default()),
            Decimal::ZERO
        );
    }

    #[test]
    fn higher_multiplier_increases_fee() {
        let cheap = estimate_fee_per_contract(dec!(0.50), &FeeModel::default());
        let expensive = estimate_fee_per_contract(
            dec!(0.50),
            &FeeModel {
                multiplier: dec!(2),
            },
        );
        assert!(expensive > cheap);
    }
}
