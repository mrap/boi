//! JWT join tokens signed by the cluster CA private key.
//!
//! Tokens embed `ca_fingerprint` (SHA-256 of CA cert DER) so a joining
//! node can pin TLS to the expected CA without TOFU (critique F-04).
//!
//! Algorithm: ES256 (ECDSA P-256 + SHA-256) — matches the CA key type
//! generated in `ca.rs`.

use std::time::{SystemTime, UNIX_EPOCH};

use jsonwebtoken::{
    decode, encode, Algorithm, DecodingKey, EncodingKey, Header, Validation,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;
use uuid::Uuid;
use x509_parser::pem::parse_x509_pem;
use x509_parser::prelude::FromDer;
use x509_parser::certificate::X509Certificate;

/// Default token TTL: 5 minutes (per F-21 in the design critique).
pub const DEFAULT_TTL_SECS: i64 = 300;

#[derive(Debug, Error)]
pub enum TokenError {
    #[error("jwt error: {0}")]
    Jwt(#[from] jsonwebtoken::errors::Error),
    #[error("system time error: {0}")]
    Time(#[from] std::time::SystemTimeError),
    #[error("pem parse error: {0}")]
    Pem(String),
    #[error("x509 parse error: {0}")]
    X509(String),
    #[error("fingerprint mismatch")]
    FingerprintMismatch,
}

/// Payload of a join token. `exp` is the standard JWT expiry claim;
/// `expires_at` mirrors it for callers that want a typed field.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct JoinTokenClaims {
    pub cluster_id: String,
    pub seed_addrs: Vec<String>,
    pub token_id: String,
    pub expires_at: i64,
    pub ca_fingerprint: String,
    pub exp: i64,
}

/// Hex-encoded SHA-256 of the CA certificate DER bytes.
pub fn ca_fingerprint(ca_cert_der: &[u8]) -> String {
    let digest = Sha256::digest(ca_cert_der);
    hex::encode(digest)
}

fn now_unix() -> Result<i64, TokenError> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64)
}

/// Extract the raw EC public key point (BIT STRING contents of the SPKI)
/// from a PEM-encoded X.509 cert. For P-256 this is 65 bytes starting with
/// 0x04 (uncompressed point) — what `jsonwebtoken::DecodingKey::from_ec_der`
/// expects.
fn ec_point_from_cert_pem(cert_pem: &str) -> Result<Vec<u8>, TokenError> {
    let (_, pem) = parse_x509_pem(cert_pem.as_bytes())
        .map_err(|e| TokenError::Pem(e.to_string()))?;
    let (_, cert) = X509Certificate::from_der(&pem.contents)
        .map_err(|e| TokenError::X509(e.to_string()))?;
    Ok(cert.public_key().subject_public_key.data.to_vec())
}

/// Mint a join token signed by the CA private key.
///
/// `ca_key_pem` — CA private key (EC PKCS#8 PEM, as produced by rcgen).
/// `ca_cert_der` — CA certificate DER bytes (used to compute fingerprint).
pub fn mint_join_token(
    ca_key_pem: &str,
    ca_cert_der: &[u8],
    cluster_id: &str,
    seed_addrs: Vec<String>,
    ttl_secs: i64,
) -> Result<String, TokenError> {
    let now = now_unix()?;
    let exp = now + ttl_secs;
    let claims = JoinTokenClaims {
        cluster_id: cluster_id.to_string(),
        seed_addrs,
        token_id: Uuid::new_v4().to_string(),
        expires_at: exp,
        ca_fingerprint: ca_fingerprint(ca_cert_der),
        exp,
    };
    let header = Header::new(Algorithm::ES256);
    let key = EncodingKey::from_ec_pem(ca_key_pem.as_bytes())?;
    Ok(encode(&header, &claims, &key)?)
}

/// Validate a join token against the CA cert (PEM).
///
/// Checks: ES256 signature against CA public key, expiry, and (if
/// `expected_fingerprint` is `Some`) that the embedded ca_fingerprint
/// matches the local CA. Returns the claims on success.
pub fn validate_token(
    token: &str,
    ca_cert_pem: &str,
    expected_fingerprint: Option<&str>,
) -> Result<JoinTokenClaims, TokenError> {
    let point = ec_point_from_cert_pem(ca_cert_pem)?;
    let key = DecodingKey::from_ec_der(&point);
    let mut validation = Validation::new(Algorithm::ES256);
    // We don't issue aud/iss; disable those.
    validation.validate_aud = false;
    validation.required_spec_claims.clear();
    validation.required_spec_claims.insert("exp".to_string());

    let data = decode::<JoinTokenClaims>(token, &key, &validation)?;
    let claims = data.claims;

    if let Some(expected) = expected_fingerprint {
        if claims.ca_fingerprint != expected {
            return Err(TokenError::FingerprintMismatch);
        }
    }
    Ok(claims)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ca::ClusterCa;

    fn fresh_ca() -> ClusterCa {
        ClusterCa::generate_ca().unwrap()
    }

    #[test]
    fn join_token_mint_and_validate_roundtrip() {
        let ca = fresh_ca();
        let der = ca.cert_der().unwrap();
        let token = mint_join_token(
            ca.key_pem(),
            &der,
            "cluster-xyz",
            vec!["127.0.0.1:7000".into()],
            DEFAULT_TTL_SECS,
        )
        .unwrap();

        let fp = ca_fingerprint(&der);
        let claims = validate_token(&token, ca.cert_pem(), Some(&fp)).unwrap();
        assert_eq!(claims.cluster_id, "cluster-xyz");
        assert_eq!(claims.seed_addrs, vec!["127.0.0.1:7000".to_string()]);
        assert_eq!(claims.ca_fingerprint, fp);
        assert!(!claims.token_id.is_empty());
    }

    #[test]
    fn join_token_expired_is_rejected() {
        let ca = fresh_ca();
        let der = ca.cert_der().unwrap();
        // Negative TTL → token expired the moment it was minted.
        let token = mint_join_token(
            ca.key_pem(),
            &der,
            "c1",
            vec!["127.0.0.1:1".into()],
            -120,
        )
        .unwrap();

        let res = validate_token(&token, ca.cert_pem(), None);
        assert!(res.is_err(), "expired token must not validate");
    }

    #[test]
    fn join_token_tampered_payload_fails_signature() {
        let ca = fresh_ca();
        let der = ca.cert_der().unwrap();
        let token = mint_join_token(
            ca.key_pem(),
            &der,
            "c1",
            vec!["127.0.0.1:1".into()],
            DEFAULT_TTL_SECS,
        )
        .unwrap();

        // Tamper one byte of the payload segment. JWT is header.payload.sig.
        let parts: Vec<&str> = token.split('.').collect();
        assert_eq!(parts.len(), 3);
        let mut payload = parts[1].to_string();
        // Flip the last char of the payload (still base64url-valid).
        let last = payload.pop().unwrap();
        let replacement = if last == 'A' { 'B' } else { 'A' };
        payload.push(replacement);
        let tampered = format!("{}.{}.{}", parts[0], payload, parts[2]);

        let res = validate_token(&tampered, ca.cert_pem(), None);
        assert!(res.is_err(), "tampered token must fail signature check");
    }

    #[test]
    fn join_token_fingerprint_mismatch_rejected() {
        let ca = fresh_ca();
        let der = ca.cert_der().unwrap();
        let token = mint_join_token(
            ca.key_pem(),
            &der,
            "c1",
            vec![],
            DEFAULT_TTL_SECS,
        )
        .unwrap();

        let mut bad = ca_fingerprint(&der);
        // Flip one hex char.
        let last = bad.pop().unwrap();
        let replacement = if last == '0' { '1' } else { '0' };
        bad.push(replacement);

        let res = validate_token(&token, ca.cert_pem(), Some(&bad));
        assert!(matches!(res, Err(TokenError::FingerprintMismatch)));
    }

    #[test]
    fn join_token_wrong_ca_rejected() {
        let ca_a = fresh_ca();
        let ca_b = fresh_ca();
        let der_a = ca_a.cert_der().unwrap();
        let token = mint_join_token(
            ca_a.key_pem(),
            &der_a,
            "c1",
            vec![],
            DEFAULT_TTL_SECS,
        )
        .unwrap();

        // Validate against a DIFFERENT CA — signature must fail.
        let res = validate_token(&token, ca_b.cert_pem(), None);
        assert!(res.is_err());
    }
}
