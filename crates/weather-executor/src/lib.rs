//! Kalshi order placement + auth.
//!
//! Kalshi v2 auth (different from Polymarket — no EIP-712, no HMAC):
//!   - Headers: `KALSHI-ACCESS-KEY`, `KALSHI-ACCESS-TIMESTAMP`,
//!     `KALSHI-ACCESS-SIGNATURE`.
//!   - Signature = base64(RSA-PSS-SHA256(timestamp + method + path)).
//!     Body is NOT included in the signature.
//!   - Private key is RSA, supplied as a PEM file.
//!
//! TODO: load PEM at startup, sign + send `POST /portfolio/orders`. Mirror
//! the polymarket bot's dry-run / live opt-in pattern.
