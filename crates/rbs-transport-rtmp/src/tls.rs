//! TLS for `rtmps://` (ADR-0127 decision 10).
//!
//! The TLS session lives on the I/O thread only: `send` and the hand-off never touch
//! it. Certificates are verified with the operating system's trust store
//! (`rustls-platform-verifier`), plus roots the session was given as an adapter
//! option. The socket stays blocking for writes, as for plain RTMP, so every write
//! flushes its TLS records before it returns: rustls's buffer never grows and a write
//! never spins.

use std::io::{self, ErrorKind, Read, Write};
use std::net::{Shutdown, TcpStream};
use std::sync::Arc;

use rbs_transport::TransportError;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, ServerName};
use rustls::{CertificateError, ClientConfig, ClientConnection, Error as TlsError};

/// Plain text rustls may hold before it is turned into records. Each write flushes,
/// so this bounds one write's chunk, not a backlog.
const PLAINTEXT_LIMIT: usize = 64 * 1024;

/// Parses the `tls_extra_roots` adapter option: one or more PEM certificates.
pub(crate) fn parse_roots(pem: &str) -> Result<Vec<CertificateDer<'static>>, String> {
    let roots = CertificateDer::pem_slice_iter(pem.as_bytes())
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("tls_extra_roots: not PEM certificates ({e})"))?;
    if roots.is_empty() {
        return Err("tls_extra_roots: no certificate in the PEM text".into());
    }
    Ok(roots)
}

/// The client configuration: TLS 1.2 and 1.3 with the ring provider, certificates
/// verified by the platform verifier with `extra` roots added. Built on the I/O
/// thread (loading the system roots may read files).
pub(crate) fn client_config(
    extra: &[CertificateDer<'static>],
) -> Result<Arc<ClientConfig>, TransportError> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let verifier = if extra.is_empty() {
        rustls_platform_verifier::Verifier::new(Arc::clone(&provider))
    } else {
        rustls_platform_verifier::Verifier::new_with_extra_roots(
            extra.iter().cloned(),
            Arc::clone(&provider),
        )
    }
    .map_err(|e| TransportError::Connection(format!("TLS certificate verifier: {e}")))?;
    let config = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| TransportError::Connection(format!("TLS: {e}")))?
        // `dangerous` only because the verifier is not the built-in WebPKI one: the
        // platform verifier verifies every certificate.
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(verifier))
        .with_no_client_auth();
    Ok(Arc::new(config))
}

/// Whether a TLS error means the server's certificate was checked and refused: a
/// rejection, never retried. Anything else (the verifier could not decide, a
/// handshake or I/O failure, a variant this list does not know) is a connection
/// error that the reconnect policy bounds.
pub(crate) fn is_rejection(error: &TlsError) -> bool {
    let TlsError::InvalidCertificate(c) = error else {
        return false;
    };
    matches!(
        c,
        CertificateError::BadEncoding
            | CertificateError::Expired
            | CertificateError::ExpiredContext { .. }
            | CertificateError::NotValidYet
            | CertificateError::NotValidYetContext { .. }
            | CertificateError::Revoked
            | CertificateError::UnknownIssuer
            | CertificateError::NotValidForName
            | CertificateError::NotValidForNameContext { .. }
            | CertificateError::InvalidPurpose
            | CertificateError::InvalidPurposeContext { .. }
            | CertificateError::BadSignature
    )
}

/// The transport error for a TLS error, with the rule of [`is_rejection`].
pub(crate) fn transport_error(error: &TlsError) -> TransportError {
    if is_rejection(error) {
        TransportError::Rejected(format!("the server's certificate is not valid: {error}"))
    } else {
        TransportError::Connection(format!("TLS: {error}"))
    }
}

/// One step of the handshake failed.
pub(crate) enum StepError {
    Io(io::Error),
    Tls(TlsError),
}

/// A TLS client session over a connected socket.
pub(crate) struct TlsWire {
    conn: ClientConnection,
    sock: TcpStream,
    /// Bytes written to the socket and not yet reported by [`TlsWire::take_sent`].
    sent: u64,
}

impl TlsWire {
    /// Starts a session to `host` (a DNS name or an IP address) over `sock`.
    pub(crate) fn new(
        config: Arc<ClientConfig>,
        host: &str,
        sock: TcpStream,
    ) -> Result<Self, TransportError> {
        let name = ServerName::try_from(host.to_owned())
            .map_err(|e| TransportError::Connection(format!("TLS server name {host:?}: {e}")))?;
        let mut conn = ClientConnection::new(config, name)
            .map_err(|e| TransportError::Connection(format!("TLS: {e}")))?;
        conn.set_buffer_limit(Some(PLAINTEXT_LIMIT));
        Ok(Self {
            conn,
            sock,
            sent: 0,
        })
    }

    pub(crate) fn socket(&self) -> &TcpStream {
        &self.sock
    }

    /// One step of the handshake: writes what is due, or reads once (a read timeout
    /// comes back as `StepError::Io` with a timeout kind). `Ok(true)` once the
    /// handshake is complete and flushed.
    pub(crate) fn handshake_step(&mut self) -> Result<bool, StepError> {
        if self.conn.wants_write() {
            self.flush_records().map_err(StepError::Io)?;
        } else if self.conn.is_handshaking() {
            match self.conn.read_tls(&mut self.sock) {
                Ok(0) => {
                    return Err(StepError::Io(io::Error::new(
                        ErrorKind::UnexpectedEof,
                        "the server closed the connection during the TLS handshake",
                    )));
                }
                Ok(_) => {
                    self.conn.process_new_packets().map_err(StepError::Tls)?;
                }
                Err(e) => return Err(StepError::Io(e)),
            }
        }
        Ok(!self.conn.is_handshaking() && !self.conn.wants_write())
    }

    /// Writes every pending TLS record. On a blocking socket this returns only when
    /// all are written or the socket failed.
    fn flush_records(&mut self) -> io::Result<()> {
        while self.conn.wants_write() {
            match self.conn.write_tls(&mut self.sock) {
                Ok(0) => return Err(ErrorKind::WriteZero.into()),
                Ok(n) => self.sent += n as u64,
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    /// Bytes put on the wire since the last call.
    pub(crate) fn take_sent(&mut self) -> u64 {
        std::mem::take(&mut self.sent)
    }

    /// Sends `close_notify` and shuts the write side down.
    pub(crate) fn shutdown_write(&mut self) {
        self.conn.send_close_notify();
        let _ = self.flush_records();
        let _ = self.sock.shutdown(Shutdown::Write);
    }
}

impl Read for TlsWire {
    /// Plain text from the server. A timeout or `WouldBlock` comes back as is; `Ok(0)`
    /// is the server's `close_notify`.
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            match self.conn.reader().read(buf) {
                Ok(n) => return Ok(n),
                Err(e) if e.kind() == ErrorKind::WouldBlock => {}
                Err(e) => return Err(e),
            }
            // Records rustls produced while reading (alerts, key updates). On the
            // non-blocking poll a full socket leaves them queued for the next write.
            match self.flush_records() {
                Ok(()) => {}
                Err(e) if e.kind() == ErrorKind::WouldBlock => {}
                Err(e) => return Err(e),
            }
            if self.conn.read_tls(&mut self.sock)? == 0 {
                return Err(io::Error::new(
                    ErrorKind::UnexpectedEof,
                    "the server closed the connection without close_notify",
                ));
            }
            self.conn
                .process_new_packets()
                .map_err(|e| io::Error::new(ErrorKind::InvalidData, e))?;
        }
    }
}

impl Write for TlsWire {
    /// Encrypts up to [`PLAINTEXT_LIMIT`] bytes of `buf` and writes the records before
    /// returning. Never `Ok(0)` for a non-empty `buf` on a blocking socket.
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.flush_records()?;
        let n = self.conn.writer().write(buf)?;
        self.flush_records()?;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.flush_records()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn certificate_errors_split_into_rejections_and_connection_errors() {
        for c in [
            CertificateError::BadEncoding,
            CertificateError::Expired,
            CertificateError::NotValidYet,
            CertificateError::Revoked,
            CertificateError::UnknownIssuer,
            CertificateError::NotValidForName,
            CertificateError::InvalidPurpose,
            CertificateError::BadSignature,
        ] {
            let e = TlsError::InvalidCertificate(c);
            assert!(is_rejection(&e), "{e:?}");
            assert!(matches!(transport_error(&e), TransportError::Rejected(_)));
        }
        for e in [
            TlsError::InvalidCertificate(CertificateError::UnknownRevocationStatus),
            TlsError::InvalidCertificate(CertificateError::ExpiredRevocationList),
            TlsError::InvalidCertificate(CertificateError::ApplicationVerificationFailure),
            TlsError::General("verifier unavailable".into()),
            TlsError::HandshakeNotComplete,
        ] {
            assert!(!is_rejection(&e), "{e:?}");
            assert!(matches!(transport_error(&e), TransportError::Connection(_)));
        }
    }

    #[test]
    fn extra_roots_must_be_pem_certificates() {
        assert!(parse_roots("").is_err());
        assert!(parse_roots("not pem").is_err());
    }
}
