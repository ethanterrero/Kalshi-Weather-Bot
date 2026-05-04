//! Kalshi REST API auth: RSA-PSS-SHA256 signing.
//!
//! Per Kalshi's docs, every authenticated request carries three headers:
//!
//!   KALSHI-ACCESS-KEY        -- the public key id (string).
//!   KALSHI-ACCESS-TIMESTAMP  -- ms since epoch, decimal string.
//!   KALSHI-ACCESS-SIGNATURE  -- base64( RSA-PSS-SHA256( timestamp + method + path ) ).
//!
//! The body is *not* covered by the signature. The path includes the
//! version prefix, e.g. `/trade-api/v2/portfolio/orders`.
//!
//! `SigningKey::<Sha256>::new` from the `rsa` crate uses MGF1-SHA256 and a
//! salt of length = hash length (32 bytes), which is the conventional PSS
//! parameterisation and matches what Kalshi accepts.

use base64::Engine;
use rsa::pkcs1::DecodeRsaPrivateKey;
use rsa::pkcs8::DecodePrivateKey;
use rsa::pss::SigningKey;
use rsa::sha2::Sha256;
use rsa::signature::{RandomizedSigner, SignatureEncoding};
use rsa::RsaPrivateKey;
use std::path::Path;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum AuthError {
    #[error("failed to read PEM: {0}")]
    Io(#[from] std::io::Error),
    #[error("PEM is not a recognisable RSA private key (tried PKCS#8 then PKCS#1)")]
    InvalidPem,
}

/// The three KALSHI-ACCESS-* headers a signed request must carry.
#[derive(Debug, Clone)]
pub struct AccessHeaders {
    pub key_id: String,
    pub timestamp_ms: String,
    pub signature: String,
}

impl AccessHeaders {
    /// Apply onto a `reqwest::RequestBuilder`. Kept as a `&self` consumer
    /// rather than `into` because the call site also wants to log the same
    /// values it just attached.
    pub fn apply(&self, builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        builder
            .header("KALSHI-ACCESS-KEY", &self.key_id)
            .header("KALSHI-ACCESS-TIMESTAMP", &self.timestamp_ms)
            .header("KALSHI-ACCESS-SIGNATURE", &self.signature)
    }
}

pub struct KalshiSigner {
    signing_key: SigningKey<Sha256>,
}

impl KalshiSigner {
    /// Read a PEM file and build a signer.
    pub fn from_pem_file(path: impl AsRef<Path>) -> Result<Self, AuthError> {
        let pem = std::fs::read_to_string(path)?;
        Self::from_pem(&pem)
    }

    /// Build a signer from a PEM string. Tries PKCS#8 first (the modern
    /// default `openssl genpkey` emits), then falls back to PKCS#1 (the
    /// legacy `openssl genrsa` shape).
    pub fn from_pem(pem: &str) -> Result<Self, AuthError> {
        let key = RsaPrivateKey::from_pkcs8_pem(pem)
            .or_else(|_| RsaPrivateKey::from_pkcs1_pem(pem))
            .map_err(|_| AuthError::InvalidPem)?;
        Ok(Self {
            signing_key: SigningKey::<Sha256>::new(key),
        })
    }

    /// Build the signing input exactly as Kalshi expects: concatenation of
    /// timestamp (decimal ms), method (uppercase, e.g. "POST"), and path
    /// (including version prefix).
    pub fn signing_string(timestamp_ms: i64, method: &str, path: &str) -> String {
        format!("{}{}{}", timestamp_ms, method, path)
    }

    /// Produce the base64 signature over `(timestamp + method + path)`. PSS
    /// is randomised, so this output changes each call even for the same
    /// inputs — the verifier accepts any valid signature.
    pub fn sign(&self, timestamp_ms: i64, method: &str, path: &str) -> String {
        let msg = Self::signing_string(timestamp_ms, method, path);
        let mut rng = rand::thread_rng();
        let signature = self.signing_key.sign_with_rng(&mut rng, msg.as_bytes());
        base64::engine::general_purpose::STANDARD.encode(signature.to_bytes())
    }

    /// Build the full set of access headers in one call.
    pub fn access_headers(
        &self,
        key_id: &str,
        timestamp_ms: i64,
        method: &str,
        path: &str,
    ) -> AccessHeaders {
        AccessHeaders {
            key_id: key_id.to_string(),
            timestamp_ms: timestamp_ms.to_string(),
            signature: self.sign(timestamp_ms, method, path),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rsa::pkcs8::{EncodePrivateKey, LineEnding};
    use rsa::pss::VerifyingKey;
    use rsa::sha2::Sha256;
    use rsa::signature::Verifier;
    use rsa::RsaPrivateKey;

    /// Generate a fast (insecure) test key + matching signer/verifier pair.
    /// 1024 bits is too weak for production but signs in single-digit
    /// milliseconds, which keeps the test suite snappy.
    fn fresh_test_signer() -> (KalshiSigner, VerifyingKey<Sha256>, String) {
        let mut rng = rand::thread_rng();
        let private = RsaPrivateKey::new(&mut rng, 1024).expect("rsa keygen");
        let public = private.to_public_key();
        let pem = private
            .to_pkcs8_pem(LineEnding::LF)
            .expect("pkcs8 encode")
            .to_string();
        let signer = KalshiSigner::from_pem(&pem).expect("from_pem");
        let verifier = VerifyingKey::<Sha256>::new(public);
        (signer, verifier, pem)
    }

    #[test]
    fn signing_string_layout_matches_kalshi_spec() {
        let s = KalshiSigner::signing_string(
            1_700_000_000_000,
            "POST",
            "/trade-api/v2/portfolio/orders",
        );
        assert_eq!(s, "1700000000000POST/trade-api/v2/portfolio/orders");
    }

    #[test]
    fn signature_round_trips_through_pss_sha256_verifier() {
        let (signer, verifier, _) = fresh_test_signer();
        let ts = 1_700_000_000_000i64;
        let method = "POST";
        let path = "/trade-api/v2/portfolio/orders";

        let sig_b64 = signer.sign(ts, method, path);
        let sig_bytes = base64::engine::general_purpose::STANDARD
            .decode(&sig_b64)
            .expect("base64 decode");
        let signature =
            rsa::pss::Signature::try_from(sig_bytes.as_slice()).expect("signature parse");
        let msg = KalshiSigner::signing_string(ts, method, path);
        verifier.verify(msg.as_bytes(), &signature).expect("verify");
    }

    #[test]
    fn signature_changes_each_call_due_to_randomised_pss_salt() {
        let (signer, _, _) = fresh_test_signer();
        let a = signer.sign(1, "GET", "/x");
        let b = signer.sign(1, "GET", "/x");
        assert_ne!(
            a, b,
            "PSS is randomised; same inputs must produce different sigs"
        );
    }

    #[test]
    fn signature_does_not_validate_for_tampered_message() {
        let (signer, verifier, _) = fresh_test_signer();
        let sig_b64 = signer.sign(1, "GET", "/x");
        let sig_bytes = base64::engine::general_purpose::STANDARD
            .decode(&sig_b64)
            .unwrap();
        let signature = rsa::pss::Signature::try_from(sig_bytes.as_slice()).unwrap();
        // Different path → different signing string → must fail to verify.
        let msg = KalshiSigner::signing_string(1, "GET", "/y");
        assert!(verifier.verify(msg.as_bytes(), &signature).is_err());
    }

    #[test]
    fn access_headers_carry_all_three_required_fields() {
        let (signer, _, _) = fresh_test_signer();
        let h = signer.access_headers("KEY-123", 12345, "POST", "/p");
        assert_eq!(h.key_id, "KEY-123");
        assert_eq!(h.timestamp_ms, "12345");
        assert!(!h.signature.is_empty());
    }

    #[test]
    fn invalid_pem_returns_an_error_rather_than_panicking() {
        match KalshiSigner::from_pem("not a real PEM") {
            Err(AuthError::InvalidPem) => {}
            Err(e) => panic!("expected InvalidPem, got {:?}", e),
            Ok(_) => panic!("garbage PEM should not load"),
        }
    }

    #[test]
    fn pkcs1_format_pem_also_loads() {
        // Generate, re-encode as PKCS#1, ensure the loader still accepts.
        use rsa::pkcs1::EncodeRsaPrivateKey;
        let mut rng = rand::thread_rng();
        let private = RsaPrivateKey::new(&mut rng, 1024).unwrap();
        let pem = private.to_pkcs1_pem(LineEnding::LF).unwrap().to_string();
        KalshiSigner::from_pem(&pem).expect("PKCS#1 PEM should load");
    }
}
