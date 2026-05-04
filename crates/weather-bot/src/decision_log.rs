//! Per-day JSONL log of every strategy decision.
//!
//! One row per market per strategy pass — both `Trade` and `NoTrade` — written
//! to `<dir>/<UTC date>.jsonl`. The columns line up with what backtests need:
//! enough to join (model probability, market state, decision) at time `t`
//! against the realized CLI outcome on the market's settlement day.
//!
//! Sparse fields are intentional. `NoTrade(NoOrderbook)` rows can't carry
//! quote-derived fields, and `NoTrade(SpreadTooWide)` rows have a spread but
//! no edge or EV. We record what the strategy actually computed and leave the
//! rest null rather than fabricating values.

use chrono::{DateTime, NaiveDate, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tokio::fs::{create_dir_all, OpenOptions};
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;

use weather_pricing::ModelPricing;
use weather_strategy::{Decision, NoTradeReason};
use weather_types::{KalshiMarket, Side, TempStat, ThresholdDirection, WeatherThreshold};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecisionRecord {
    pub ts: DateTime<Utc>,
    pub ticker: String,
    pub city: String,
    pub settlement_station: String,
    /// Daily high vs daily low — backtests need this to pick which CLI
    /// extreme to compare against.
    pub stat: TempStat,
    /// `at_or_above` or `at_or_below` — direction of the YES condition.
    pub direction: ThresholdDirection,
    /// Strike temperature in °F.
    pub strike_temperature_f: i32,
    /// Settlement date (the climate day Kalshi resolves on, not `ts`).
    pub resolution_date: NaiveDate,
    pub horizon_days: i64,
    pub forecast_temp_f: i32,
    pub sigma_f: f64,
    pub model_p_yes: Decimal,
    pub yes_bid: Option<Decimal>,
    pub yes_ask: Option<Decimal>,
    pub spread: Option<Decimal>,
    /// `"trade"` or `"no_trade"`.
    pub decision: String,
    /// Tag of the no-trade reason (`"no_orderbook"`, `"spread_too_wide"`,
    /// `"edge_below_min"`, `"ev_below_gate"`), or `None` on `Trade`.
    pub reason: Option<String>,
    pub side: Option<Side>,
    pub limit_price: Option<Decimal>,
    pub contracts: Option<u32>,
    pub fee_estimate: Option<Decimal>,
    pub raw_edge: Option<Decimal>,
    pub net_ev_per_contract: Option<Decimal>,
    pub market_p_implied: Option<Decimal>,
}

/// Build a `DecisionRecord` from the inputs available at the strategy-loop
/// callsite. The `now` arg is wired in for tests; production callers pass
/// `Utc::now()`.
pub fn record_from(
    market: &KalshiMarket,
    threshold: &WeatherThreshold,
    pricing: &ModelPricing,
    decision: &Decision,
    now: DateTime<Utc>,
) -> DecisionRecord {
    let yes_bid = market.yes_bid;
    let yes_ask = market.yes_ask;
    let spread = match (yes_ask, yes_bid) {
        (Some(a), Some(b)) => Some(a - b),
        _ => None,
    };

    let mut rec = DecisionRecord {
        ts: now,
        ticker: market.ticker.clone(),
        city: threshold.city.clone(),
        settlement_station: pricing.settlement_station.to_string(),
        stat: threshold.stat,
        direction: threshold.direction,
        strike_temperature_f: threshold.temperature_f,
        resolution_date: threshold.date,
        horizon_days: pricing.horizon_days,
        forecast_temp_f: pricing.forecast_temp_f,
        sigma_f: pricing.sigma_f,
        model_p_yes: pricing.yes_probability,
        yes_bid,
        yes_ask,
        spread,
        decision: String::new(),
        reason: None,
        side: None,
        limit_price: None,
        contracts: None,
        fee_estimate: None,
        raw_edge: None,
        net_ev_per_contract: None,
        market_p_implied: None,
    };

    match decision {
        Decision::Trade(sig, ev) => {
            rec.decision = "trade".into();
            rec.side = Some(sig.side);
            rec.limit_price = Some(sig.limit_price);
            rec.contracts = Some(sig.contracts);
            rec.fee_estimate = Some(ev.fee_estimate);
            rec.raw_edge = Some(ev.raw_edge);
            rec.net_ev_per_contract = Some(ev.net_ev_per_contract);
            rec.market_p_implied = Some(ev.market_implied_probability);
        }
        Decision::NoTrade(reason) => {
            rec.decision = "no_trade".into();
            rec.reason = Some(no_trade_tag(reason).to_string());
            match reason {
                NoTradeReason::NoOrderbook | NoTradeReason::SpreadTooWide { .. } => {}
                NoTradeReason::EdgeBelowMin { raw_edge, .. } => {
                    rec.raw_edge = Some(*raw_edge);
                }
                NoTradeReason::EvBelowGate { net_ev, .. } => {
                    rec.net_ev_per_contract = Some(*net_ev);
                }
            }
        }
    }
    rec
}

fn no_trade_tag(reason: &NoTradeReason) -> &'static str {
    match reason {
        NoTradeReason::NoOrderbook => "no_orderbook",
        NoTradeReason::SpreadTooWide { .. } => "spread_too_wide",
        NoTradeReason::EdgeBelowMin { .. } => "edge_below_min",
        NoTradeReason::EvBelowGate { .. } => "ev_below_gate",
    }
}

/// Appends `DecisionRecord`s to `<dir>/<UTC date>.jsonl`. A single mutex
/// serialises writers so concurrent strategy passes (and any future
/// per-city tasks) can't interleave bytes mid-line.
pub struct DecisionLogger {
    dir: Option<PathBuf>,
    write_lock: Mutex<()>,
}

impl DecisionLogger {
    /// `dir = None` disables persistence (`record` becomes a no-op).
    pub fn new(dir: Option<PathBuf>) -> Self {
        Self {
            dir,
            write_lock: Mutex::new(()),
        }
    }

    pub async fn record(&self, record: &DecisionRecord) -> std::io::Result<()> {
        let Some(dir) = self.dir.as_ref() else {
            return Ok(());
        };
        let _guard = self.write_lock.lock().await;
        create_dir_all(dir).await?;
        let path = dir.join(format!("{}.jsonl", record.ts.date_naive()));
        let mut file = OpenOptions::new()
            .append(true)
            .create(true)
            .open(&path)
            .await?;
        let mut line = serde_json::to_string(record)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        line.push('\n');
        file.write_all(line.as_bytes()).await?;
        // tokio::fs::File buffers internally and does NOT flush on drop, so
        // a downstream reader (test, log shipper) may see an empty file
        // momentarily without an explicit flush. Cheap; one line per
        // record means there's no batching to preserve.
        file.flush().await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use rust_decimal_macros::dec;
    use weather_strategy::EvBreakdown;
    use weather_types::Signal;

    fn ts() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 5, 3, 18, 30, 0).unwrap()
    }

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

    fn pricing() -> ModelPricing {
        ModelPricing {
            yes_probability: dec!(0.65),
            forecast_temp_f: 78,
            sigma_f: 2.0,
            horizon_days: 1,
            settlement_station: "KNYC",
        }
    }

    fn ny_high_75() -> WeatherThreshold {
        WeatherThreshold {
            city: "NY".into(),
            stat: TempStat::DailyHigh,
            direction: ThresholdDirection::AtOrAbove,
            temperature_f: 75,
            date: chrono::NaiveDate::from_ymd_opt(2026, 7, 4).unwrap(),
        }
    }

    fn trade_decision() -> Decision {
        let sig = Signal {
            id: uuid::Uuid::nil(),
            market_ticker: "KXHIGHNY-26JUL04-T75".into(),
            side: Side::Yes,
            limit_price: dec!(0.50),
            contracts: 7,
            model_yes_probability: dec!(0.65),
            edge: dec!(0.15),
            generated_at: ts(),
        };
        let ev = EvBreakdown {
            side: Side::Yes,
            price: dec!(0.50),
            model_probability: dec!(0.65),
            market_implied_probability: dec!(0.50),
            spread: dec!(0.05),
            fee_estimate: dec!(0.012),
            safety_buffer: dec!(0.01),
            raw_edge: dec!(0.15),
            net_ev_per_contract: dec!(0.085),
            required_net_ev: dec!(0.01),
        };
        Decision::Trade(sig, Box::new(ev))
    }

    #[test]
    fn trade_record_populates_signal_and_ev() {
        let m = market(Some(dec!(0.45)), Some(dec!(0.50)));
        let p = pricing();
        let d = trade_decision();
        let r = record_from(&m, &ny_high_75(), &p, &d, ts());

        assert_eq!(r.decision, "trade");
        assert!(r.reason.is_none());
        assert_eq!(r.side, Some(Side::Yes));
        assert_eq!(r.limit_price, Some(dec!(0.50)));
        assert_eq!(r.contracts, Some(7));
        assert_eq!(r.raw_edge, Some(dec!(0.15)));
        assert_eq!(r.net_ev_per_contract, Some(dec!(0.085)));
        assert_eq!(r.market_p_implied, Some(dec!(0.50)));
        assert_eq!(r.fee_estimate, Some(dec!(0.012)));
        assert_eq!(r.spread, Some(dec!(0.05)));
        assert_eq!(r.city, "NY");
        assert_eq!(r.settlement_station, "KNYC");
        assert_eq!(r.model_p_yes, dec!(0.65));
    }

    #[test]
    fn no_orderbook_record_is_sparse() {
        let m = market(None, None);
        let p = pricing();
        let d = Decision::NoTrade(NoTradeReason::NoOrderbook);
        let r = record_from(&m, &ny_high_75(), &p, &d, ts());

        assert_eq!(r.decision, "no_trade");
        assert_eq!(r.reason.as_deref(), Some("no_orderbook"));
        assert!(r.yes_bid.is_none());
        assert!(r.yes_ask.is_none());
        assert!(r.spread.is_none());
        assert!(r.side.is_none());
        assert!(r.limit_price.is_none());
        assert!(r.raw_edge.is_none());
        assert!(r.net_ev_per_contract.is_none());
    }

    #[test]
    fn spread_too_wide_keeps_spread_drops_edge() {
        let m = market(Some(dec!(0.10)), Some(dec!(0.50)));
        let p = pricing();
        let d = Decision::NoTrade(NoTradeReason::SpreadTooWide {
            spread: dec!(0.40),
            max: dec!(0.10),
        });
        let r = record_from(&m, &ny_high_75(), &p, &d, ts());

        assert_eq!(r.reason.as_deref(), Some("spread_too_wide"));
        assert_eq!(r.spread, Some(dec!(0.40)));
        assert!(r.raw_edge.is_none());
        assert!(r.net_ev_per_contract.is_none());
    }

    #[test]
    fn edge_below_min_carries_raw_edge_only() {
        let m = market(Some(dec!(0.50)), Some(dec!(0.54)));
        let p = pricing();
        let d = Decision::NoTrade(NoTradeReason::EdgeBelowMin {
            raw_edge: dec!(0.01),
            min: dec!(0.05),
        });
        let r = record_from(&m, &ny_high_75(), &p, &d, ts());

        assert_eq!(r.reason.as_deref(), Some("edge_below_min"));
        assert_eq!(r.raw_edge, Some(dec!(0.01)));
        assert!(r.net_ev_per_contract.is_none());
    }

    #[test]
    fn ev_below_gate_carries_net_ev_only() {
        let m = market(Some(dec!(0.45)), Some(dec!(0.50)));
        let p = pricing();
        let d = Decision::NoTrade(NoTradeReason::EvBelowGate {
            net_ev: dec!(0.003),
            required: dec!(0.20),
        });
        let r = record_from(&m, &ny_high_75(), &p, &d, ts());

        assert_eq!(r.reason.as_deref(), Some("ev_below_gate"));
        assert_eq!(r.net_ev_per_contract, Some(dec!(0.003)));
        assert!(r.raw_edge.is_none());
    }

    #[tokio::test]
    async fn writes_jsonl_line_to_dated_file() {
        // Disposable per-test directory so concurrent test runs don't collide.
        let dir = std::env::temp_dir().join(format!(
            "weather-bot-decisions-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
        ));
        let logger = DecisionLogger::new(Some(dir.clone()));
        let m = market(Some(dec!(0.45)), Some(dec!(0.50)));
        let r = record_from(&m, &ny_high_75(), &pricing(), &trade_decision(), ts());

        logger.record(&r).await.expect("record should succeed");

        let expected_path = dir.join("2026-05-03.jsonl");
        let contents = std::fs::read_to_string(&expected_path).expect("file should exist");
        let mut lines = contents.lines();
        let line = lines.next().expect("at least one line written");
        assert!(lines.next().is_none(), "exactly one line per record");

        let parsed: serde_json::Value = serde_json::from_str(line).expect("valid JSON");
        assert_eq!(parsed["ticker"], "KXHIGHNY-26JUL04-T75");
        assert_eq!(parsed["decision"], "trade");
        assert_eq!(parsed["side"], "yes");
        assert_eq!(parsed["model_p_yes"], "0.65");
        assert_eq!(parsed["settlement_station"], "KNYC");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn disabled_logger_is_no_op() {
        let logger = DecisionLogger::new(None);
        let m = market(None, None);
        let r = record_from(
            &m,
            &ny_high_75(),
            &pricing(),
            &Decision::NoTrade(NoTradeReason::NoOrderbook),
            ts(),
        );
        logger
            .record(&r)
            .await
            .expect("disabled logger should succeed silently");
    }

    #[tokio::test]
    async fn appends_multiple_records_to_same_dated_file() {
        let dir = std::env::temp_dir().join(format!(
            "weather-bot-decisions-append-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
        ));
        let logger = DecisionLogger::new(Some(dir.clone()));
        let m = market(Some(dec!(0.45)), Some(dec!(0.50)));
        let r = record_from(&m, &ny_high_75(), &pricing(), &trade_decision(), ts());
        logger.record(&r).await.unwrap();
        logger.record(&r).await.unwrap();

        let path = dir.join("2026-05-03.jsonl");
        let contents = std::fs::read_to_string(&path).unwrap();
        assert_eq!(contents.lines().count(), 2);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
