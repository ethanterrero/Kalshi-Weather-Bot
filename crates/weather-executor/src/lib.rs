//! Kalshi order placement + RSA-PSS-SHA256 auth.
//!
//! Kalshi v2 auth (different from Polymarket — no EIP-712, no HMAC):
//!   - Headers: `KALSHI-ACCESS-KEY`, `KALSHI-ACCESS-TIMESTAMP`,
//!     `KALSHI-ACCESS-SIGNATURE`.
//!   - Signature = base64( RSA-PSS-SHA256( timestamp + method + path ) ).
//!     Body is NOT included in the signature.
//!   - Private key is RSA, supplied as a PEM file (PKCS#8 preferred,
//!     PKCS#1 also accepted).
//!
//! v1 of this crate ships the auth path and request shape end-to-end, but
//! every place_order() call short-circuits on a hard-coded `never_send`
//! flag until we've validated payloads in real demo conditions.

pub mod auth;
pub mod orders;

pub use auth::{AccessHeaders, AuthError, KalshiSigner};
pub use orders::{
    price_to_cents, KalshiOrderClient, OrderAction, OrderEcho, OrderError, OrderRequest,
    OrderResponse, OrderType,
};
