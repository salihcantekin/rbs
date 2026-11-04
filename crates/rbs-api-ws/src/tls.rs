//! TLS for network access (ADR-0046): the server identity (certificate chain and key),
//! the key pin clients check, and the generated key and certificate.
//!
//! - Clients pin the key: `sha256//` and the base64 SHA-256 of its SubjectPublicKeyInfo
//!   (the form of curl's `--pinnedpubkey`). Renewing the certificate keeps the key, so
//!   the pin survives renewals and address changes.
//! - The certificate's SHA-256 fingerprint (colon-separated upper-case hex, as browsers
//!   and Node show it) is shown too; it changes at each renewal.
//! - TLS 1.2 and 1.3 on rustls' `ring` provider, ALPN `http/1.1`, no session tickets
//!   and no session cache: there are few clients, and the server keeps less state.

use std::net::IpAddr;
use std::sync::Arc;

use rustls::ServerConfig;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use sha2::{Digest, Sha256};

/// Days a generated certificate is valid: Apple's limit for TLS server certificates is
/// 825 days from `notBefore` to `notAfter`, which starts a day early here.
pub const CERTIFICATE_DAYS: i64 = 824;

/// A server identity, ready to serve: the certificate chain, its key and the TLS
/// settings built from them.
#[derive(Clone)]
pub struct TlsIdentity {
    config: Arc<ServerConfig>,
    key_pin: String,
    fingerprint: String,
}

impl std::fmt::Debug for TlsIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TlsIdentity")
            .field("key_pin", &self.key_pin)
            .field("fingerprint", &self.fingerprint)
            .finish_non_exhaustive()
    }
}

impl PartialEq for TlsIdentity {
    fn eq(&self, other: &Self) -> bool {
        self.key_pin == other.key_pin && self.fingerprint == other.fingerprint
    }
}

impl Eq for TlsIdentity {}

impl TlsIdentity {
    /// The identity in `cert_chain_pem` (the server's certificate first) and `key_pem`.
    /// Refused, in words, when either cannot be read or the key is not the
    /// certificate's.
    pub fn from_pem(cert_chain_pem: &str, key_pem: &str) -> Result<TlsIdentity, String> {
        let chain = CertificateDer::pem_slice_iter(cert_chain_pem.as_bytes())
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("the certificate cannot be read ({e})"))?;
        let leaf = chain
            .first()
            .ok_or("the certificate file holds no certificate")?;
        let fingerprint = cert_fingerprint(leaf);
        let key = PrivateKeyDer::from_pem_slice(key_pem.as_bytes())
            .map_err(|e| format!("the key cannot be read ({e})"))?;
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let signing = provider
            .key_provider
            .load_private_key(key.clone_key())
            .map_err(|e| format!("the key cannot be used ({e})"))?;
        let spki = signing
            .public_key()
            .ok_or("the key's public part cannot be read")?;
        let key_pin = key_pin(spki.as_ref());
        let certified = rustls::sign::CertifiedKey::new(chain.clone(), signing);
        certified
            .keys_match()
            .map_err(|_| "the key does not belong to the certificate".to_owned())?;
        let mut config = ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .map_err(|e| format!("TLS settings: {e}"))?
            .with_no_client_auth()
            .with_single_cert(chain, key)
            .map_err(|e| format!("the certificate cannot be used ({e})"))?;
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        config.send_tls13_tickets = 0;
        config.session_storage = Arc::new(rustls::server::NoServerSessionStorage {});
        Ok(TlsIdentity {
            config: Arc::new(config),
            key_pin,
            fingerprint,
        })
    }

    /// What clients pin: `sha256//<base64>` of the key's SubjectPublicKeyInfo.
    pub fn key_pin(&self) -> &str {
        &self.key_pin
    }

    /// The SHA-256 fingerprint of the server's certificate, `AB:CD:...`.
    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    pub(crate) fn server_config(&self) -> Arc<ServerConfig> {
        self.config.clone()
    }
}

/// `sha256//<base64>` of a DER SubjectPublicKeyInfo.
pub fn key_pin(spki_der: &[u8]) -> String {
    format!(
        "sha256//{}",
        crate::handshake::base64(&Sha256::digest(spki_der))
    )
}

/// The SHA-256 fingerprint of a DER certificate as colon-separated upper-case hex.
pub fn cert_fingerprint(cert_der: &[u8]) -> String {
    Sha256::digest(cert_der)
        .iter()
        .map(|b| format!("{b:02X}"))
        .collect::<Vec<_>>()
        .join(":")
}

/// A generated key and certificate, as PEM.
#[derive(Clone, PartialEq, Eq)]
pub struct Generated {
    /// The certificate.
    pub cert_pem: String,
    /// The private key (PKCS#8). Store it readable by the current user only.
    pub key_pem: String,
    /// When the certificate stops being valid, in seconds since the Unix epoch.
    pub not_after: i64,
}

impl std::fmt::Debug for Generated {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Generated")
            .field("not_after", &self.not_after)
            .finish_non_exhaustive()
    }
}

/// A new ECDSA P-256 key and a self-signed certificate for it, valid from a day before
/// `now` (seconds since the Unix epoch) for [`CERTIFICATE_DAYS`]: `CA:false`, the
/// `serverAuth` extended key usage, and `names` and `addresses` as subject alternative
/// names.
pub fn generate(names: &[String], addresses: &[IpAddr], now: i64) -> Result<Generated, String> {
    let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256)
        .map_err(|e| format!("the system could not make a key ({e})"))?;
    certificate(key, names, addresses, now)
}

/// A new certificate for the key in `key_pem`, as [`generate`] makes it: the key pin
/// stays the same.
pub fn renew(
    key_pem: &str,
    names: &[String],
    addresses: &[IpAddr],
    now: i64,
) -> Result<Generated, String> {
    let key =
        rcgen::KeyPair::from_pem(key_pem).map_err(|e| format!("the key cannot be read ({e})"))?;
    certificate(key, names, addresses, now)
}

fn certificate(
    key: rcgen::KeyPair,
    names: &[String],
    addresses: &[IpAddr],
    now: i64,
) -> Result<Generated, String> {
    use rcgen::{
        CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose, IsCa,
        KeyUsagePurpose, SanType,
    };
    const DAY: i64 = 86_400;
    let mut params = CertificateParams::default();
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, "RBS remote control");
    params.distinguished_name = dn;
    let mut sans = Vec::new();
    for n in names {
        let name = n
            .clone()
            .try_into()
            .map_err(|_| format!("{n:?} cannot be a certificate name"))?;
        sans.push(SanType::DnsName(name));
    }
    sans.extend(
        addresses
            .iter()
            .map(|a| SanType::IpAddress(a.to_canonical())),
    );
    params.subject_alt_names = sans;
    params.is_ca = IsCa::ExplicitNoCa;
    params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    let start = now.div_euclid(DAY) - 1;
    let end = start + CERTIFICATE_DAYS;
    let (y, m, d) = ymd(start)?;
    params.not_before = rcgen::date_time_ymd(y, m, d);
    let (y, m, d) = ymd(end)?;
    params.not_after = rcgen::date_time_ymd(y, m, d);
    let cert = params
        .self_signed(&key)
        .map_err(|e| format!("the certificate could not be made ({e})"))?;
    Ok(Generated {
        cert_pem: cert.pem(),
        key_pem: key.serialize_pem(),
        not_after: end * DAY,
    })
}

/// The date of `days` since the Unix epoch, for `rcgen::date_time_ymd`.
fn ymd(days: i64) -> Result<(i32, u8, u8), String> {
    let (y, m, d) = civil_from_days(days);
    let y = i32::try_from(y)
        .ok()
        .filter(|y| (1..=9999).contains(y))
        .ok_or("the date is out of range")?;
    Ok((y, m, d))
}

/// The proleptic Gregorian date of `days` since 1970-01-01 (Howard Hinnant's
/// `civil_from_days`).
fn civil_from_days(days: i64) -> (i64, u8, u8) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    // `m` is 1..=12 and `d` 1..=31 by construction.
    (y, m as u8, d as u8)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)] // Tests fail by panicking.
mod tests {
    use super::*;

    #[test]
    fn dates_convert() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
        assert_eq!(civil_from_days(19_723), (2024, 1, 1));
        assert_eq!(civil_from_days(19_782), (2024, 2, 29));
        assert_eq!(civil_from_days(20_396), (2025, 11, 4));
    }

    #[test]
    fn the_pin_and_fingerprint_have_their_fixed_forms() {
        // SHA-256 of nothing.
        assert_eq!(
            key_pin(b""),
            "sha256//47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU="
        );
        let f = cert_fingerprint(b"");
        assert!(f.starts_with("E3:B0:C4:42:98:FC:1C:14"), "{f}");
        assert_eq!(f.len(), 32 * 3 - 1);
    }

    #[test]
    fn a_generated_identity_serves_and_renewal_keeps_the_key_pin() {
        let names = vec!["localhost".to_owned(), "studio.local".to_owned()];
        let addresses: Vec<IpAddr> = vec!["192.168.1.7".parse().unwrap()];
        let now = 1_793_750_400; // 2026-11-04
        let g = generate(&names, &addresses, now).unwrap();
        assert!(g.key_pem.contains("PRIVATE KEY"));
        assert_eq!(g.not_after, (now / 86_400 - 1 + CERTIFICATE_DAYS) * 86_400);
        assert!(!format!("{g:?}").contains("PRIVATE"));
        let id = TlsIdentity::from_pem(&g.cert_pem, &g.key_pem).unwrap();
        assert!(id.key_pin().starts_with("sha256//") && id.key_pin().len() == 52);
        assert!(!format!("{id:?}").contains("PRIVATE"));

        let r = renew(&g.key_pem, &names, &[], now + 800 * 86_400).unwrap();
        let renewed = TlsIdentity::from_pem(&r.cert_pem, &r.key_pem).unwrap();
        assert_eq!(renewed.key_pin(), id.key_pin());
        assert_ne!(renewed.fingerprint(), id.fingerprint());

        let other = generate(&names, &addresses, now).unwrap();
        assert_ne!(
            TlsIdentity::from_pem(&other.cert_pem, &other.key_pem)
                .unwrap()
                .key_pin(),
            id.key_pin()
        );
        let err = TlsIdentity::from_pem(&g.cert_pem, &other.key_pem).unwrap_err();
        assert!(err.contains("does not belong"), "{err}");
        assert!(TlsIdentity::from_pem("", &g.key_pem).is_err());
        assert!(TlsIdentity::from_pem(&g.cert_pem, "nonsense").is_err());
    }
}
