//! Cluster Certificate Authority.
//!
//! Generates a self-signed root CA (ECDSA P-256), persists it to disk
//! (`<dir>/ca.crt` + `ca.key`), and signs leaf node certs via
//! [`Cluster Ca::mint_node_cert`].

use std::fs;
use std::path::{Path, PathBuf};

use rcgen::{
    BasicConstraints, Certificate, CertificateParams, DnType, IsCa, KeyPair,
    PKCS_ECDSA_P256_SHA256,
};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum CaError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("rcgen error: {0}")]
    Rcgen(#[from] rcgen::Error),
}

/// PEM-encoded cert + key bundle.
#[derive(Debug, Clone)]
pub struct CertBundle {
    pub cert_pem: String,
    pub key_pem: String,
}

/// In-memory cluster CA capable of signing leaf certificates.
pub struct ClusterCa {
    cert: Certificate,
    cert_pem: String,
    key_pem: String,
}

impl ClusterCa {
    /// Generate a fresh self-signed root CA.
    pub fn generate_ca() -> Result<Self, CaError> {
        let key_pair = KeyPair::generate(&PKCS_ECDSA_P256_SHA256)?;
        let mut params = CertificateParams::new(vec![]);
        params.alg = &PKCS_ECDSA_P256_SHA256;
        params
            .distinguished_name
            .push(DnType::CommonName, "boi cluster CA");
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params.key_pair = Some(key_pair);

        let cert = Certificate::from_params(params)?;
        let cert_pem = cert.serialize_pem()?;
        let key_pem = cert.serialize_private_key_pem();
        Ok(Self {
            cert,
            cert_pem,
            key_pem,
        })
    }

    /// Persist CA cert + key as PEM files in `dir` (creates dir if needed).
    pub fn persist(&self, dir: &Path) -> Result<(), CaError> {
        fs::create_dir_all(dir)?;
        fs::write(dir.join("ca.crt"), &self.cert_pem)?;
        fs::write(dir.join("ca.key"), &self.key_pem)?;
        Ok(())
    }

    /// Load a previously persisted CA from `dir`.
    pub fn load(dir: &Path) -> Result<Self, CaError> {
        let cert_pem = fs::read_to_string(dir.join("ca.crt"))?;
        let key_pem = fs::read_to_string(dir.join("ca.key"))?;
        let key_pair = KeyPair::from_pem(&key_pem)?;
        let params = CertificateParams::from_ca_cert_pem(&cert_pem, key_pair)?;
        let cert = Certificate::from_params(params)?;
        // Re-serialize for identity output; the on-disk pem is authoritative.
        Ok(Self {
            cert,
            cert_pem,
            key_pem,
        })
    }

    /// Convenience: load if `dir/ca.crt` exists, otherwise generate + persist.
    pub fn load_or_generate(dir: &Path) -> Result<Self, CaError> {
        if dir.join("ca.crt").exists() {
            Self::load(dir)
        } else {
            let ca = Self::generate_ca()?;
            ca.persist(dir)?;
            Ok(ca)
        }
    }

    pub fn cert_pem(&self) -> &str {
        &self.cert_pem
    }

    pub fn key_pem(&self) -> &str {
        &self.key_pem
    }

    /// CA cert in DER format (used for fingerprinting).
    pub fn cert_der(&self) -> Result<Vec<u8>, CaError> {
        Ok(self.cert.serialize_der()?)
    }

    /// Mint a leaf node certificate signed by this CA.
    /// CN = node_id, SAN includes node_id and "localhost".
    pub fn mint_node_cert(&self, node_id: &str) -> Result<CertBundle, CaError> {
        let leaf_key = KeyPair::generate(&PKCS_ECDSA_P256_SHA256)?;
        let mut params =
            CertificateParams::new(vec![node_id.to_string(), "localhost".to_string()]);
        params.alg = &PKCS_ECDSA_P256_SHA256;
        params
            .distinguished_name
            .push(DnType::CommonName, node_id);
        params.is_ca = IsCa::NoCa;
        params.key_pair = Some(leaf_key);

        let leaf = Certificate::from_params(params)?;
        let cert_pem = leaf.serialize_pem_with_signer(&self.cert)?;
        let key_pem = leaf.serialize_private_key_pem();
        Ok(CertBundle { cert_pem, key_pem })
    }
}

/// Default on-disk location for the cluster CA (used by callers that want
/// `~/.boi/cluster/`). Returns `None` if home cannot be determined.
pub fn default_ca_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .map(|h| h.join(".boi").join("cluster"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use x509_parser::pem::parse_x509_pem;
    use x509_parser::prelude::*;

    fn parse_pem_cert(pem_str: &str) -> Vec<u8> {
        let (_, pem) = parse_x509_pem(pem_str.as_bytes()).unwrap();
        assert_eq!(pem.label, "CERTIFICATE");
        pem.contents
    }

    #[test]
    fn ca_generate_persist_load_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let ca = ClusterCa::generate_ca().unwrap();
        ca.persist(dir.path()).unwrap();

        let loaded = ClusterCa::load(dir.path()).unwrap();
        // Loaded cert PEM should equal what was written.
        let on_disk = std::fs::read_to_string(dir.path().join("ca.crt")).unwrap();
        assert_eq!(loaded.cert_pem(), on_disk);
    }

    #[test]
    fn ca_load_or_generate_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let ca1 = ClusterCa::load_or_generate(dir.path()).unwrap();
        let ca2 = ClusterCa::load_or_generate(dir.path()).unwrap();
        // Second call must load, not regenerate.
        assert_eq!(ca1.cert_pem(), ca2.cert_pem());
    }

    #[test]
    fn ca_mints_leaf_that_chains_to_ca() {
        let ca = ClusterCa::generate_ca().unwrap();
        let bundle = ca.mint_node_cert("node-abc").unwrap();

        // Parse leaf and CA, verify leaf was signed by CA public key.
        let leaf_der = parse_pem_cert(&bundle.cert_pem);
        let ca_der = parse_pem_cert(ca.cert_pem());

        let (_, leaf) = X509Certificate::from_der(&leaf_der).unwrap();
        let (_, ca_x509) = X509Certificate::from_der(&ca_der).unwrap();

        // Issuer of leaf must equal subject of CA.
        assert_eq!(leaf.issuer(), ca_x509.subject());

        // Verify leaf signature with CA public key.
        leaf.verify_signature(Some(ca_x509.public_key()))
            .expect("leaf cert must verify against CA public key");

        // Leaf has expected CN.
        let cn = leaf
            .subject()
            .iter_common_name()
            .next()
            .unwrap()
            .as_str()
            .unwrap();
        assert_eq!(cn, "node-abc");
    }

    #[test]
    fn ca_mint_does_not_chain_to_different_ca() {
        let ca_a = ClusterCa::generate_ca().unwrap();
        let ca_b = ClusterCa::generate_ca().unwrap();
        let leaf = ca_a.mint_node_cert("node-x").unwrap();

        let leaf_der = parse_pem_cert(&leaf.cert_pem);
        let ca_b_der = parse_pem_cert(ca_b.cert_pem());
        let (_, leaf_x) = X509Certificate::from_der(&leaf_der).unwrap();
        let (_, ca_b_x) = X509Certificate::from_der(&ca_b_der).unwrap();

        // Leaf signed by CA A must NOT verify against CA B.
        assert!(leaf_x.verify_signature(Some(ca_b_x.public_key())).is_err());
    }
}
