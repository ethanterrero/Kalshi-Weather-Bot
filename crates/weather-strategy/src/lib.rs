//! Edge-aware signal generator.
//!
//! Workflow per market:
//!   1. Read the orderbook bid/ask from the scanner snapshot.
//!   2. Combine with model probability (`weather-pricing`) to compute YES-side
//!      and NO-side expected value per contract, *net of estimated fees*.
//!   3. If either side clears `min_edge` and beats `safety_buffer +
//!      half_spread + fee`, emit a `Signal`.
//!   4. Otherwise, return a `NoTrade` reason — surfaced in dry-run logs so
//!      the operator can see *why* nothing fired.
//!
//! Fee model — Kalshi help (https://help.kalshi.com/en/articles/13823805-fees):
//!   fee_per_100_contracts = round_up_to_cent( fee_multiplier * 0.07 *
//!       contracts/100 * price * (1 - price) )
//! For a single contract this works out to roughly `7¢ * p*(1-p) * mult`,
//! peaking at 1.75¢ at p=0.5 and falling toward 0 at the edges. Per-100
//! ranges $0.07–$1.75 match the help-center quote.

pub mod fees;

use chrono::Utc;
use rust_decimal::prelude::*;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use uuid::Uuid;

use weather_config::StrategyConfig;
use weather_pricing::ModelPricing;
use weather_types::{KalshiMarket, Side, Signal};

pub use fees::{estimate_fee_per_contract, FeeModel};

/// Best bid/ask in dollars for both sides.
#[derive(Debug, Clone, Copy)]
pub struct OrderbookQuote {
    pub yes_bid: Option<Decimal>,
    pub yes_ask: Option<Decimal>,
}

impl OrderbookQuote {
    pub fn from_market(m: &KalshiMarket) -> Self {
        Self {
            yes_bid: m.yes_bid,
            yes_ask: m.yes_ask,
        }
    }

    /// YES ask is the price you pay to buy YES. NO ask, by Kalshi binary
    /// no-arb, equals (1 - yes_bid). The Kalshi orderbook docs make this
    /// explicit: "implied YES ask = 1 - best NO bid; YES spread = ask - bid".
    pub fn yes_ask(&self) -> Option<Decimal> {
        self.yes_ask
    }

    /// NO ask = 1 - yes_bid (you sell YES at the bid, equivalent to buying NO).
    pub fn no_ask(&self) -> Option<Decimal> {
        self.yes_bid.map(|b| Decimal::ONE - b)
    }

    /// YES bid-ask spread, when both sides are quoted.
    pub fn yes_spread(&self) -> Option<Decimal> {
        match (self.yes_ask, self.yes_bid) {
            (Some(a), Some(b)) => Some(a - b),
            _ => None,
        }
    }
}

/// What the strategy decided about a market, and why. The explicit `NoTrade`
/// branch is what dry-run logs print so the operator can see edge framework
/// math rather than just silence.
///
/// `EvBreakdown` is boxed because it's ~10× the size of `NoTradeReason`, and
/// most decisions in the loop are `NoTrade`. Box auto-derefs at field-access
/// callsites so consumer code doesn't have to change shape.
#[derive(Debug, Clone)]
pub enum Decision {
    Trade(Signal, Box<EvBreakdown>),
    NoTrade(NoTradeReason),
}

/// Full EV math for a market, kept around so logs can include every input.
#[derive(Debug, Clone)]
pub struct EvBreakdown {
    pub side: Side,
    /// Limit price we'd post (the resting opposite-side ask).
    pub price: Decimal,
    pub model_probability: Decimal,
    pub market_implied_probability: Decimal,
    pub spread: Decimal,
    pub fee_estimate: Decimal,
    pub safety_buffer: Decimal,
    /// Raw edge in probability terms: |model_p - market_p|.
    pub raw_edge: Decimal,
    /// EV per contract net of fee, in dollars.
    pub net_ev_per_contract: Decimal,
    /// Required net EV (= safety_buffer) to clear the gate. The gate fires
    /// when `net_ev_per_contract >= safety_buffer`.
    pub required_net_ev: Decimal,
}

#[derive(Debug, Clone)]
pub enum NoTradeReason {
    NoOrderbook,
    SpreadTooWide {
        spread: Decimal,
        max: Decimal,
    },
    EdgeBelowMin {
        raw_edge: Decimal,
        min: Decimal,
    },
    EvBelowGate {
        net_ev: Decimal,
        required: Decimal,
    },
    /// Limit price is outside the configured `[min_price, max_price]` band.
    /// Tail-bracket contracts (cheap or near-1.00) carry asymmetric blow-up
    /// risk: a $0.05 contract priced at 50% true still needs an ~83% true
    /// hit rate to break even after fees, and one losing tail event wipes
    /// out months of small wins. Both ends are excluded.
    PriceOutOfBand {
        price: Decimal,
        min: Decimal,
        max: Decimal,
    },
    /// NWS issued the forecast we'd be trading off less than the lockout
    /// window ago. Arb bots reprice within seconds of an NWS update; we
    /// sit out the first 30 minutes to avoid being adversely selected.
    /// The decision is gated externally (in the strategy loop) rather
    /// than inside `decide()` because the forecast freshness clock isn't
    /// part of the orderbook + pricing inputs `decide` already takes.
    ForecastTooFresh {
        age_secs: i64,
        lockout_secs: i64,
    },
}

impl std::fmt::Display for NoTradeReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NoTradeReason::NoOrderbook => write!(f, "no orderbook quotes available"),
            NoTradeReason::SpreadTooWide { spread, max } => {
                write!(f, "spread {} > max {}", spread, max)
            }
            NoTradeReason::EdgeBelowMin { raw_edge, min } => {
                write!(f, "raw edge {} < min {}", raw_edge, min)
            }
            NoTradeReason::EvBelowGate { net_ev, required } => {
                write!(f, "net EV {} < required {}", net_ev, required)
            }
            NoTradeReason::PriceOutOfBand { price, min, max } => {
                write!(f, "price {} outside band [{}, {}]", price, min, max)
            }
            NoTradeReason::ForecastTooFresh {
                age_secs,
                lockout_secs,
            } => {
                write!(
                    f,
                    "NWS forecast issued {}s ago, inside {}s lockout",
                    age_secs, lockout_secs
                )
            }
        }
    }
}

/// Edge-aware decision for one market. `contract_size_cap` is passed
/// separately because position sizing is the risk layer's job — the strategy
/// only proposes the side and the price.
pub fn decide(
    market: &KalshiMarket,
    pricing: &ModelPricing,
    cfg: &StrategyConfig,
    fees: &FeeModel,
) -> Decision {
    let quote = OrderbookQuote::from_market(market);
    let yes_ask = match quote.yes_ask() {
        Some(p) => p,
        None => return Decision::NoTrade(NoTradeReason::NoOrderbook),
    };
    let no_ask = match quote.no_ask() {
        Some(p) => p,
        None => return Decision::NoTrade(NoTradeReason::NoOrderbook),
    };
    let spread = quote.yes_spread().unwrap_or(Decimal::ZERO);
    if spread > cfg.max_spread {
        return Decision::NoTrade(NoTradeReason::SpreadTooWide {
            spread,
            max: cfg.max_spread,
        });
    }

    let model_p = pricing.yes_probability;
    let yes_ev = side_ev(Side::Yes, yes_ask, model_p, fees);
    let no_ev = side_ev(Side::No, no_ask, Decimal::ONE - model_p, fees);

    // Pick the side with the larger raw edge. Both EVs are net-of-fee.
    let (side, price, market_implied, raw_edge, net_ev) = if yes_ev.raw_edge >= no_ev.raw_edge {
        (Side::Yes, yes_ask, yes_ask, yes_ev.raw_edge, yes_ev.net_ev)
    } else {
        (
            Side::No,
            no_ask,
            Decimal::ONE - no_ask,
            no_ev.raw_edge,
            no_ev.net_ev,
        )
    };

    if raw_edge < cfg.min_edge {
        return Decision::NoTrade(NoTradeReason::EdgeBelowMin {
            raw_edge,
            min: cfg.min_edge,
        });
    }

    // Price band check: tail-bracket contracts (cheap or near-1.00) carry
    // asymmetric blow-up risk. Apply after edge so the JSONL can show
    // there *was* an edge but we declined for risk reasons.
    if price < cfg.min_price || price > cfg.max_price {
        return Decision::NoTrade(NoTradeReason::PriceOutOfBand {
            price,
            min: cfg.min_price,
            max: cfg.max_price,
        });
    }

    let required = cfg.safety_buffer;
    if net_ev < required {
        return Decision::NoTrade(NoTradeReason::EvBelowGate { net_ev, required });
    }

    let fee_estimate = estimate_fee_per_contract(price, fees);
    let breakdown = EvBreakdown {
        side,
        price,
        model_probability: if matches!(side, Side::Yes) {
            model_p
        } else {
            Decimal::ONE - model_p
        },
        market_implied_probability: market_implied,
        spread,
        fee_estimate,
        safety_buffer: cfg.safety_buffer,
        raw_edge,
        net_ev_per_contract: net_ev,
        required_net_ev: required,
    };

    let contracts = kelly_contracts(net_ev, price, cfg.kelly_fraction);
    let signal = Signal {
        id: Uuid::new_v4(),
        market_ticker: market.ticker.clone(),
        side,
        limit_price: price,
        contracts,
        model_yes_probability: model_p,
        edge: raw_edge,
        generated_at: Utc::now(),
    };
    Decision::Trade(signal, Box::new(breakdown))
}

struct SideEv {
    raw_edge: Decimal,
    net_ev: Decimal,
}

/// EV per contract for one side, net of fee. `our_p` is the probability the
/// side we'd take resolves true (so for the YES side it's model_p, for the
/// NO side it's 1 - model_p). `price` is what we pay per contract.
fn side_ev(_side: Side, price: Decimal, our_p: Decimal, fees: &FeeModel) -> SideEv {
    let market_implied = price; // Kalshi prices ARE the market's implied probability.
    let raw_edge = (our_p - market_implied).max(Decimal::ZERO);
    // Gross EV per contract = our_p * (1 - price) - (1 - our_p) * price = our_p - price.
    let gross_ev = our_p - price;
    let fee = estimate_fee_per_contract(price, fees);
    SideEv {
        raw_edge,
        net_ev: gross_ev - fee,
    }
}

/// Kelly-fractional contract count given net EV and a Kelly multiplier.
/// We don't have bankroll wired in here (that's the risk layer's job), so
/// this returns a unit-scale "if you had $X bankroll, take f*Kelly% of it"
/// hint that the risk layer translates into actual contracts. v1 keeps it
/// simple: floor of `1 / price` contracts, scaled by Kelly fraction. The
/// risk manager applies the real cap.
fn kelly_contracts(net_ev: Decimal, price: Decimal, kelly_frac: Decimal) -> u32 {
    if net_ev <= Decimal::ZERO || price <= Decimal::ZERO {
        return 0;
    }
    // Kelly fraction of full Kelly = f* = (p*b - q) / b, simplified to
    // (model_edge / (1 - price)). For v1, use 1 contract scaled by Kelly
    // fraction — the risk layer enforces $-caps.
    let raw = (net_ev / price).max(dec!(0));
    let scaled = raw * kelly_frac * dec!(100); // unit-scale hint
    scaled.to_u32().unwrap_or(1).max(1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use rust_decimal_macros::dec;

    fn market(yes_bid: Option<Decimal>, yes_ask: Option<Decimal>) -> KalshiMarket {
        KalshiMarket {
            ticker: "KXHIGHNY-26JUL04-T75".into(),
            series_ticker: "KXHIGHNY".into(),
            event_ticker: "KXHIGHNY-26JUL04".into(),
            title: "high temp".into(),
            yes_sub_title: "75° or above".into(),
            resolution_date: chrono::NaiveDate::from_ymd_opt(2026, 7, 4).unwrap(),
            close_time: Utc.with_ymd_and_hms(2026, 7, 5, 5, 0, 0).unwrap(),
            yes_ask,
            yes_bid,
            last_price: None,
            volume: dec!(0),
            open_interest: dec!(0),
        }
    }

    fn pricing(p_yes: Decimal) -> ModelPricing {
        ModelPricing {
            yes_probability: p_yes,
            forecast_temp_f: 78,
            sigma_f: 2.0,
            horizon_days: 1,
            settlement_station: "KNYC",
        }
    }

    fn cfg() -> StrategyConfig {
        StrategyConfig {
            min_edge: dec!(0.05),
            kelly_fraction: dec!(0.25),
            safety_buffer: dec!(0.01),
            fee_multiplier: dec!(1),
            max_spread: dec!(0.10),
            min_price: dec!(0.20),
            max_price: dec!(0.92),
        }
    }

    #[test]
    fn orderbook_no_quote_is_no_trade() {
        let m = market(None, None);
        let p = pricing(dec!(0.8));
        let d = decide(&m, &p, &cfg(), &FeeModel::default());
        assert!(matches!(d, Decision::NoTrade(NoTradeReason::NoOrderbook)));
    }

    #[test]
    fn implied_no_ask_is_one_minus_yes_bid() {
        let m = market(Some(dec!(0.40)), Some(dec!(0.55)));
        let q = OrderbookQuote::from_market(&m);
        assert_eq!(q.no_ask(), Some(dec!(0.60)));
        assert_eq!(q.yes_spread(), Some(dec!(0.15)));
    }

    #[test]
    fn wide_spread_is_no_trade() {
        // YES spread = 0.50 - 0.10 = 0.40, far above max 0.10.
        let m = market(Some(dec!(0.10)), Some(dec!(0.50)));
        let p = pricing(dec!(0.99));
        let d = decide(&m, &p, &cfg(), &FeeModel::default());
        match d {
            Decision::NoTrade(NoTradeReason::SpreadTooWide { spread, max }) => {
                assert_eq!(spread, dec!(0.40));
                assert_eq!(max, dec!(0.10));
            }
            other => panic!("expected SpreadTooWide, got {:?}", other),
        }
    }

    #[test]
    fn small_edge_below_min_is_no_trade() {
        // Model P = 0.55, YES ask = 0.54 → raw edge 0.01, below min 0.05.
        let m = market(Some(dec!(0.50)), Some(dec!(0.54)));
        let p = pricing(dec!(0.55));
        let d = decide(&m, &p, &cfg(), &FeeModel::default());
        assert!(matches!(
            d,
            Decision::NoTrade(NoTradeReason::EdgeBelowMin { .. })
        ));
    }

    #[test]
    fn clear_yes_edge_fires_signal() {
        // Model P = 0.85, YES ask = 0.50 → 35¢ raw edge; even with fees +
        // safety buffer this trades.
        let m = market(Some(dec!(0.45)), Some(dec!(0.50)));
        let p = pricing(dec!(0.85));
        let d = decide(&m, &p, &cfg(), &FeeModel::default());
        match d {
            Decision::Trade(sig, ev) => {
                assert_eq!(sig.side, Side::Yes);
                assert_eq!(sig.limit_price, dec!(0.50));
                assert_eq!(ev.market_implied_probability, dec!(0.50));
                assert!(ev.raw_edge > dec!(0.30));
                assert!(ev.net_ev_per_contract > dec!(0.20));
                assert!(sig.contracts >= 1);
            }
            other => panic!("expected Trade, got {:?}", other),
        }
    }

    #[test]
    fn clear_no_edge_fires_signal_on_no_side() {
        // Model says YES is unlikely (P=0.10), market YES bid = 0.45 → NO ask
        // = 0.55, NO probability = 0.90. NO is the side with edge.
        let m = market(Some(dec!(0.45)), Some(dec!(0.50)));
        let p = pricing(dec!(0.10));
        let d = decide(&m, &p, &cfg(), &FeeModel::default());
        match d {
            Decision::Trade(sig, ev) => {
                assert_eq!(sig.side, Side::No);
                assert_eq!(sig.limit_price, dec!(0.55));
                // We're buying NO at 55¢ when our P(NO)=0.90 → ~35¢ edge.
                assert!(ev.raw_edge > dec!(0.30));
            }
            other => panic!("expected Trade on NO side, got {:?}", other),
        }
    }

    #[test]
    fn ev_below_gate_blocks_marginal_edges() {
        // Edge 5¢ exactly hits min_edge but raw fee + buffer eats most of it.
        // Set buffer high so the gate triggers.
        let m = market(Some(dec!(0.45)), Some(dec!(0.50)));
        let p = pricing(dec!(0.55));
        let mut c = cfg();
        c.safety_buffer = dec!(0.20);
        let d = decide(&m, &p, &c, &FeeModel::default());
        assert!(
            matches!(d, Decision::NoTrade(NoTradeReason::EvBelowGate { .. })),
            "got {:?}",
            d
        );
    }

    #[test]
    fn cheap_contract_below_price_floor_is_no_trade() {
        // Model P(YES) = 0.85, YES ask = 0.10. Massive raw edge (75¢) and
        // huge gross EV — but the price is below the configured floor of
        // 0.20, so the asymmetric tail-blow-up risk gate fires.
        let m = market(Some(dec!(0.05)), Some(dec!(0.10)));
        let p = pricing(dec!(0.85));
        let d = decide(&m, &p, &cfg(), &FeeModel::default());
        match d {
            Decision::NoTrade(NoTradeReason::PriceOutOfBand { price, min, max }) => {
                assert_eq!(price, dec!(0.10));
                assert_eq!(min, dec!(0.20));
                assert_eq!(max, dec!(0.92));
            }
            other => panic!("expected PriceOutOfBand, got {:?}", other),
        }
    }

    #[test]
    fn pricey_contract_above_price_ceiling_is_no_trade() {
        // Model P(YES) = 1.0, YES ask = 0.95. Raw edge = 5¢ (clears
        // min_edge), but the ceiling at 0.92 kicks in before EvBelowGate.
        let m = market(Some(dec!(0.93)), Some(dec!(0.95)));
        let p = pricing(dec!(1.0));
        let d = decide(&m, &p, &cfg(), &FeeModel::default());
        assert!(
            matches!(d, Decision::NoTrade(NoTradeReason::PriceOutOfBand { .. })),
            "got {:?}",
            d
        );
    }

    #[test]
    fn price_band_widening_lets_an_otherwise_fine_trade_through() {
        // Same setup that fails by default; widen the band to allow it.
        let m = market(Some(dec!(0.05)), Some(dec!(0.10)));
        let p = pricing(dec!(0.85));
        let mut c = cfg();
        c.min_price = dec!(0.05);
        let d = decide(&m, &p, &c, &FeeModel::default());
        assert!(matches!(d, Decision::Trade(_, _)), "got {:?}", d);
    }
}
