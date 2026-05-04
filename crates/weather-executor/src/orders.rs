//! `POST /portfolio/orders` against Kalshi.
//!
//! Two-stage rollout per the roadmap:
//!   - PR 1 (this one): build the request, sign it, and *log* it. Even
//!     when `mode = live` and creds are loaded, a hard-coded `never_send`
//!     flag keeps the actual `http.send()` short-circuited. Lets us walk
//!     the auth path in real conditions before risking any capital.
//!   - PR 2 (later): flip `never_send` off behind an explicit env var,
//!     start placing live orders behind the risk caps from Phase 6.
//!
//! Order shape mirrors Kalshi's REST docs: `count` contracts, `yes_price`
//! or `no_price` in cents (0..=100), `post_only=true` to ensure we never
//! cross the spread accidentally. `client_order_id` is a UUID we mint so
//! retries are idempotent on Kalshi's side.

use chrono::Utc;
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use rust_decimal::RoundingStrategy;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::{info, warn};
use uuid::Uuid;

use weather_types::{Side, Signal};

use crate::auth::{AuthError, KalshiSigner};

#[derive(Debug, Error)]
pub enum OrderError {
    #[error("auth: {0}")]
    Auth(#[from] AuthError),
    #[error("HTTP: {0}")]
    Http(#[from] reqwest::Error),
    #[error("Kalshi {status} for {path}: {body}")]
    Status {
        status: u16,
        path: String,
        body: String,
    },
    #[error("JSON: {0}")]
    Json(#[from] serde_json::Error),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrderType {
    Limit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrderAction {
    Buy,
    Sell,
}

/// Body of `POST /trade-api/v2/portfolio/orders`. v1 is buy-side only
/// (we open a position by buying YES or NO; closing is a separate flow).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderRequest {
    pub ticker: String,
    pub client_order_id: Uuid,
    #[serde(rename = "type")]
    pub order_type: OrderType,
    pub action: OrderAction,
    pub side: Side,
    /// Number of contracts.
    pub count: u32,
    /// YES limit price in cents (0–100). Present iff `side = yes`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub yes_price: Option<u32>,
    /// NO limit price in cents. Present iff `side = no`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub no_price: Option<u32>,
    /// Always true for v1 — we sit on the resting opposite-side ask, never
    /// cross the spread.
    pub post_only: bool,
}

impl OrderRequest {
    /// Build a buy-side limit order from a strategy `Signal`. Limit price
    /// is converted from dollar `Decimal` to integer cents (rounding away
    /// from zero), since Kalshi's API takes integer cent prices.
    pub fn from_signal(signal: &Signal) -> Self {
        let cents = price_to_cents(signal.limit_price);
        let (yes_price, no_price) = match signal.side {
            Side::Yes => (Some(cents), None),
            Side::No => (None, Some(cents)),
        };
        OrderRequest {
            ticker: signal.market_ticker.clone(),
            client_order_id: signal.id,
            order_type: OrderType::Limit,
            action: OrderAction::Buy,
            side: signal.side,
            count: signal.contracts,
            yes_price,
            no_price,
            post_only: true,
        }
    }
}

/// Convert a 0.0–1.0 `Decimal` price into an integer cent count clamped
/// to 1..=99. Kalshi rejects prices at the extremes.
pub fn price_to_cents(price: Decimal) -> u32 {
    let cents = (price * Decimal::from(100))
        .round_dp_with_strategy(0, RoundingStrategy::MidpointAwayFromZero)
        .to_u32()
        .unwrap_or(0);
    cents.clamp(1, 99)
}

/// Subset of Kalshi's `POST /portfolio/orders` 200 response we currently
/// care about.
#[derive(Debug, Clone, Deserialize)]
pub struct OrderResponse {
    pub order: OrderEcho,
}

#[derive(Debug, Clone, Deserialize)]
pub struct OrderEcho {
    pub order_id: Option<String>,
    pub client_order_id: Option<Uuid>,
    pub status: Option<String>,
}

pub struct KalshiOrderClient {
    http: reqwest::Client,
    base_url: String,
    key_id: String,
    signer: KalshiSigner,
    /// Hard kill-switch on real network sends. Constructed `true` until
    /// the auth path has been walked end-to-end against demo with
    /// payloads we trust. Only flippable by code edit, not config.
    never_send: bool,
}

impl KalshiOrderClient {
    pub fn new(base_url: String, key_id: String, signer: KalshiSigner) -> Self {
        Self {
            http: reqwest::Client::new(),
            base_url,
            key_id,
            signer,
            never_send: true,
        }
    }

    /// Disable the hard kill-switch. Intentional code-only API: requires
    /// the caller to write `client.allow_real_sends()` rather than read a
    /// config value, so a stray env var can't accidentally turn the bot
    /// live. Keep separate PRs for "auth wired" vs "actually placing".
    pub fn allow_real_sends(&mut self) {
        self.never_send = false;
    }

    pub fn never_send(&self) -> bool {
        self.never_send
    }

    /// Build, sign, log — and (if `never_send=false`) send the order.
    /// Returns `Ok(None)` when the kill-switch is on; `Ok(Some(_))` only
    /// after a successful HTTP roundtrip.
    pub async fn place_order(
        &self,
        req: &OrderRequest,
    ) -> Result<Option<OrderResponse>, OrderError> {
        let path = "/trade-api/v2/portfolio/orders";
        let url = format!("{}/portfolio/orders", self.base_url);
        let body = serde_json::to_vec(req)?;
        let timestamp_ms = Utc::now().timestamp_millis();
        let headers = self
            .signer
            .access_headers(&self.key_id, timestamp_ms, "POST", path);

        info!(
            ticker = %req.ticker,
            client_order_id = %req.client_order_id,
            count = req.count,
            yes_price = ?req.yes_price,
            no_price = ?req.no_price,
            post_only = req.post_only,
            timestamp_ms,
            never_send = self.never_send,
            "kalshi order: ready to send"
        );

        if self.never_send {
            warn!("kalshi order: never_send=true; suppressing actual HTTP request");
            return Ok(None);
        }

        let req_builder = self
            .http
            .post(&url)
            .header("Content-Type", "application/json")
            .body(body);
        let resp = headers.apply(req_builder).send().await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(OrderError::Status {
                status: status.as_u16(),
                path: path.to_string(),
                body,
            });
        }
        let parsed: OrderResponse = resp.json().await?;
        Ok(Some(parsed))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use rsa::pkcs8::{EncodePrivateKey, LineEnding};
    use rsa::RsaPrivateKey;
    use rust_decimal_macros::dec;
    use uuid::Uuid;

    fn test_signer() -> KalshiSigner {
        let mut rng = rand::thread_rng();
        let private = RsaPrivateKey::new(&mut rng, 1024).unwrap();
        let pem = private.to_pkcs8_pem(LineEnding::LF).unwrap().to_string();
        KalshiSigner::from_pem(&pem).unwrap()
    }

    fn signal(side: Side, price: Decimal, contracts: u32) -> Signal {
        Signal {
            id: Uuid::new_v4(),
            market_ticker: "KXHIGHNY-26JUL04-T75".into(),
            side,
            limit_price: price,
            contracts,
            model_yes_probability: dec!(0.65),
            edge: dec!(0.15),
            generated_at: Utc::now(),
        }
    }

    #[test]
    fn yes_signal_serialises_with_yes_price_only() {
        let s = signal(Side::Yes, dec!(0.50), 7);
        let req = OrderRequest::from_signal(&s);
        assert_eq!(req.side, Side::Yes);
        assert_eq!(req.yes_price, Some(50));
        assert!(req.no_price.is_none());

        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains(r#""yes_price":50"#), "got {}", json);
        assert!(!json.contains("no_price"), "got {}", json);
        assert!(json.contains(r#""side":"yes""#), "got {}", json);
        assert!(json.contains(r#""action":"buy""#), "got {}", json);
        assert!(json.contains(r#""type":"limit""#), "got {}", json);
        assert!(json.contains(r#""post_only":true"#), "got {}", json);
    }

    #[test]
    fn no_signal_serialises_with_no_price_only() {
        let s = signal(Side::No, dec!(0.55), 3);
        let req = OrderRequest::from_signal(&s);
        assert_eq!(req.side, Side::No);
        assert!(req.yes_price.is_none());
        assert_eq!(req.no_price, Some(55));
    }

    #[test]
    fn price_to_cents_clamps_to_one_through_ninety_nine() {
        assert_eq!(price_to_cents(dec!(0.50)), 50);
        assert_eq!(price_to_cents(dec!(0.005)), 1); // rounds to 1, then floor of clamp
        assert_eq!(price_to_cents(dec!(0)), 1);
        assert_eq!(price_to_cents(dec!(1.0)), 99);
        assert_eq!(price_to_cents(dec!(0.999)), 99);
        // 0.495 rounds to 50, not 49 (midpoint-away-from-zero).
        assert_eq!(price_to_cents(dec!(0.495)), 50);
    }

    #[test]
    fn never_send_is_default_true_and_suppresses_send() {
        let client = KalshiOrderClient::new(
            "https://demo-api.kalshi.co/trade-api/v2".to_string(),
            "KEY-123".to_string(),
            test_signer(),
        );
        assert!(client.never_send());

        // We don't actually want to hit the network in unit tests; this
        // test asserts the construction default. The async path is
        // exercised in the integration test below (also gated never_send=true,
        // so it still doesn't hit the network).
    }

    #[tokio::test]
    async fn place_order_with_never_send_returns_none_without_network() {
        let client = KalshiOrderClient::new(
            "http://localhost:1/trade-api/v2".to_string(), // unreachable on purpose
            "KEY-123".to_string(),
            test_signer(),
        );
        let req = OrderRequest::from_signal(&signal(Side::Yes, dec!(0.50), 1));
        let res = client.place_order(&req).await.unwrap();
        assert!(
            res.is_none(),
            "never_send=true must short-circuit before HTTP"
        );
    }

    #[test]
    fn allow_real_sends_flips_the_kill_switch() {
        let mut client = KalshiOrderClient::new(
            "https://demo-api.kalshi.co/trade-api/v2".to_string(),
            "KEY-123".to_string(),
            test_signer(),
        );
        assert!(client.never_send());
        client.allow_real_sends();
        assert!(!client.never_send());
    }
}
