//! A test certificate authority and a server certificate for `127.0.0.1`, made fresh
//! for each test (rcgen), so the RTMPS tests need no files and no system root.

use std::sync::Arc;

use rcgen::{
    BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair,
    KeyUsagePurpose,
};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};

/// A server identity and the PEM of the authority that signed it.
pub struct Identity {
    /// The authority's certificate (PEM): the root a client must trust.
    pub ca_pem: String,
    cert: CertificateDer<'static>,
    key: Vec<u8>,
}

impl Identity {
    /// A certificate for `127.0.0.1` signed by a new authority.
    pub fn new() -> Self {
        Self::for_names(&["127.0.0.1"])
    }

    /// A certificate for `names` (none of them `127.0.0.1` makes a name mismatch).
    pub fn for_names(names: &[&str]) -> Self {
        let ca_key = KeyPair::generate().unwrap();
        let mut ca = CertificateParams::new(Vec::<String>::new()).unwrap();
        ca.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        ca.distinguished_name
            .push(DnType::CommonName, "RBS test authority");
        ca.key_usages = vec![
            KeyUsagePurpose::KeyCertSign,
            KeyUsagePurpose::CrlSign,
            KeyUsagePurpose::DigitalSignature,
        ];
        let ca_cert = ca.self_signed(&ca_key).unwrap();
        let issuer = Issuer::new(ca, ca_key);

        let key = KeyPair::generate().unwrap();
        let mut params =
            CertificateParams::new(names.iter().map(|n| (*n).to_owned()).collect::<Vec<_>>())
                .unwrap();
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        let cert = params.signed_by(&key, &issuer).unwrap();
        Self {
            ca_pem: ca_cert.pem(),
            cert: cert.der().clone(),
            key: key.serialize_der(),
        }
    }

    /// The server side: this identity's certificate and key.
    pub fn server_config(&self) -> Arc<rustls::ServerConfig> {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(self.key.clone()));
        let config = rustls::ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(vec![self.cert.clone()], key)
            .unwrap();
        Arc::new(config)
    }
}
