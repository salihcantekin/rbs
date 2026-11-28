//! The DTLS certificate of one pairing: ECDSA P-256 with SHA-256, self-signed, made
//! with rcgen on ring (ADR-0046's provider). The same shape as dimpl's own generator,
//! which needs aws-lc-sys.

use rcgen::{CertificateParams, DistinguishedName, DnType, IsCa, KeyPair, PKCS_ECDSA_P256_SHA256};
use ring::rand::{SecureRandom, SystemRandom};
use str0m_proto::crypto::dtls::DtlsCert;

/// Why a certificate could not be made.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CertificateFailed;

/// A new key pair and certificate, valid for one year from now. The common name says
/// nothing about the user or the computer.
pub(crate) fn self_signed() -> Result<DtlsCert, CertificateFailed> {
    let key_pair = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).map_err(|_| CertificateFailed)?;
    let mut params = CertificateParams::new(Vec::<String>::new()).map_err(|_| CertificateFailed)?;
    let mut name = DistinguishedName::new();
    name.push(DnType::CommonName, "RBS guest");
    params.distinguished_name = name;
    params.is_ca = IsCa::NoCa;
    let not_before = time::OffsetDateTime::now_utc() - time::Duration::days(1);
    params.not_before = not_before;
    params.not_after = not_before + time::Duration::days(366);
    // Unique serial numbers across processes and machines (Firefox needs it; see the
    // note in dimpl's generator).
    let mut serial = [0u8; 16];
    SystemRandom::new()
        .fill(&mut serial)
        .map_err(|_| CertificateFailed)?;
    serial[0] &= 0x7f; // A positive DER integer.
    params.serial_number = Some(serial.to_vec().into());
    let cert = params
        .self_signed(&key_pair)
        .map_err(|_| CertificateFailed)?;
    Ok(DtlsCert {
        certificate: cert.der().to_vec(),
        private_key: key_pair.serialize_der(),
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn each_certificate_is_new() {
        let a = self_signed().unwrap();
        let b = self_signed().unwrap();
        assert_ne!(a.certificate, b.certificate);
        assert_ne!(a.private_key, b.private_key);
        assert!(!a.certificate.windows(4).any(|w| w == b"DTLS"));
    }
}
