//! The crypto provider str0m runs on: DTLS through dimpl's RustCrypto backend, SRTP,
//! STUN's HMAC-SHA1 and the fingerprint hashes on RustCrypto crates, certificates
//! through rcgen on ring. No OpenSSL and no aws-lc (ADR-0046, ADR-0139 decision 3).
//!
//! Adapted from `str0m-rust-crypto` 0.7.0 (MIT OR Apache-2.0), whose only difference
//! is certificate generation: that crate enables dimpl's `rcgen` feature, which pulls
//! aws-lc-sys.

// The adapted code keeps upstream's shape so a later str0m update diffs cleanly.
#![allow(clippy::collapsible_if, clippy::expect_used, missing_docs)]

pub(crate) mod certificate;
mod dtls;
mod sha1;
mod sha256;
mod srtp;

use std::sync::Arc;

use str0m_proto::crypto::CryptoProvider;

/// The provider for every str0m instance of this adapter.
pub(crate) fn provider() -> Arc<CryptoProvider> {
    static SRTP: srtp::RustCryptoSrtpProvider = srtp::RustCryptoSrtpProvider;
    static SHA1_HMAC: sha1::RustCryptoSha1HmacProvider = sha1::RustCryptoSha1HmacProvider;
    static SHA1: sha1::RustCryptoSha1Provider = sha1::RustCryptoSha1Provider;
    static SHA256: sha256::RustCryptoSha256Provider = sha256::RustCryptoSha256Provider;
    static DTLS: dtls::RustCryptoDtlsProvider = dtls::RustCryptoDtlsProvider;
    Arc::new(CryptoProvider {
        srtp_provider: &SRTP,
        sha1_hmac_provider: &SHA1_HMAC,
        sha1_provider: &SHA1,
        sha256_provider: &SHA256,
        dtls_provider: &DTLS,
    })
}

/// SHA-256 of `data` (the check phrase digest, ADR-0139 decision 6).
pub(crate) fn sha256(data: &[u8]) -> [u8; 32] {
    use sha2::Digest;
    sha2::Sha256::digest(data).into()
}
