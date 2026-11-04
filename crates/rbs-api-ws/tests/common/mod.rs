//! A minimal blocking RFC 6455 client for the tests, over plain TCP or over TLS with
//! the server's key pinned (ADR-0046), as RBS clients connect.

#![allow(dead_code)] // Each test file uses part of it.

use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::net::{IpAddr, Shutdown, SocketAddr, TcpStream, UdpSocket};
use std::sync::Arc;
use std::time::Duration;

use rbs_api::conformance::{Client, ClientEvent, ConnectOptions, LIMIT};
use rbs_api_ws::{SUBPROTOCOL, accept_key, close_reason};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::WebPkiSupportedAlgorithms;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, ClientConnection, DigitallySignedStruct, SignatureScheme, StreamOwned};

pub const KEY: &str = "dGhlIHNhbXBsZSBub25jZQ==";

/// How a test client speaks TLS.
#[derive(Debug, Clone, Default)]
pub struct Tls {
    /// The key pin it accepts (`sha256//...`).
    pub pin: String,
    /// TLS 1.2 only (otherwise 1.3).
    pub tls12: bool,
    /// Sends every byte in its own `write` (TCP segment), to test reassembly.
    pub trickle: bool,
}

impl Tls {
    pub fn pinned(pin: &str) -> Self {
        Self {
            pin: pin.to_owned(),
            ..Self::default()
        }
    }
}

pub trait Stream: Read + Write + Send {}
impl<T: Read + Write + Send> Stream for T {}

/// Writes one byte per call.
struct Trickle(TcpStream);

impl Read for Trickle {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.0.read(buf)
    }
}

impl Write for Trickle {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match buf.first() {
            Some(b) => self.0.write(std::slice::from_ref(b)),
            None => Ok(0),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.0.flush()
    }
}

/// Accepts the server only if its key matches the pin; signatures are checked as
/// usual.
#[derive(Debug)]
struct Pinned {
    pin: String,
    algorithms: WebPkiSupportedAlgorithms,
}

impl ServerCertVerifier for Pinned {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let cert = webpki::EndEntityCert::try_from(end_entity)
            .map_err(|e| rustls::Error::General(format!("bad certificate: {e}")))?;
        if rbs_api_ws::tls::key_pin(cert.subject_public_key_info().as_ref()) == self.pin {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::General("the key pin does not match".into()))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algorithms.supported_schemes()
    }
}

fn client_config(tls: &Tls) -> Arc<ClientConfig> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let algorithms = provider.signature_verification_algorithms;
    let versions: &[&rustls::SupportedProtocolVersion] = if tls.tls12 {
        &[&rustls::version::TLS12]
    } else {
        &[&rustls::version::TLS13]
    };
    let mut config = ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(versions)
        .unwrap()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(Pinned {
            pin: tls.pin.clone(),
            algorithms,
        }))
        .with_no_client_auth();
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Arc::new(config)
}

/// A TCP connection to `addr`, wrapped in TLS when `tls` is given; the second value
/// is the raw socket, for timeouts and shutdown.
pub fn connect(
    addr: SocketAddr,
    tls: Option<&Tls>,
) -> Result<(Box<dyn Stream>, TcpStream), String> {
    let tcp = TcpStream::connect(addr).map_err(|e| e.to_string())?;
    tcp.set_read_timeout(Some(LIMIT)).unwrap();
    tcp.set_nodelay(true).unwrap();
    let raw = tcp.try_clone().unwrap();
    let stream: Box<dyn Stream> = match tls {
        None => Box::new(tcp),
        Some(t) => {
            let conn =
                ClientConnection::new(client_config(t), ServerName::IpAddress(addr.ip().into()))
                    .unwrap();
            if t.trickle {
                Box::new(StreamOwned::new(conn, Trickle(tcp)))
            } else {
                Box::new(StreamOwned::new(conn, tcp))
            }
        }
    };
    Ok((stream, raw))
}

/// The upgrade request for `addr` with `options`.
pub fn upgrade_request(addr: SocketAddr, options: &ConnectOptions) -> String {
    let mut req = format!(
        "GET / HTTP/1.1\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: {KEY}\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Protocol: {SUBPROTOCOL}\r\n"
    );
    if !options.no_host {
        let host = options.host.clone().unwrap_or_else(|| addr.to_string());
        req.push_str(&format!("Host: {host}\r\n"));
    }
    if let Some(o) = &options.origin {
        req.push_str(&format!("Origin: {o}\r\n"));
    }
    req.push_str("\r\n");
    req
}

/// Sends `request` and reads the whole answer, to the end of the stream.
pub fn exchange(addr: SocketAddr, tls: Option<&Tls>, request: &str) -> Result<String, String> {
    let (mut s, _raw) = connect(addr, tls)?;
    s.write_all(request.as_bytes()).map_err(|e| e.to_string())?;
    let mut answer = Vec::new();
    match s.read_to_end(&mut answer) {
        Ok(_) => {}
        // rustls reports a peer that hung up without `close_notify`; what came before
        // is still the answer.
        Err(e) if e.kind() == ErrorKind::UnexpectedEof => {}
        Err(e) => return Err(e.to_string()),
    }
    Ok(String::from_utf8_lossy(&answer).into_owned())
}

/// This machine's address on its default route, when it has one that is not
/// loopback: connecting to it is a connection from "another computer" as far as the
/// server can tell. No packet is sent.
pub fn remote_address() -> Option<IpAddr> {
    let s = UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect("198.51.100.1:9").ok()?;
    let ip = s.local_addr().ok()?.ip();
    (!ip.is_loopback() && !ip.is_unspecified()).then_some(ip)
}

pub struct WsClient {
    stream: BufReader<Box<dyn Stream>>,
    raw: TcpStream,
}

impl WsClient {
    /// Connects and upgrades; `Err` with the status line when refused.
    pub fn open(
        addr: SocketAddr,
        options: &ConnectOptions,
        tls: Option<&Tls>,
    ) -> Result<Self, String> {
        let (mut stream, raw) = connect(addr, tls)?;
        stream
            .write_all(upgrade_request(addr, options).as_bytes())
            .map_err(|e| e.to_string())?;
        let mut stream = BufReader::new(stream);
        let mut status = String::new();
        stream.read_line(&mut status).map_err(|e| e.to_string())?;
        let mut accept = None;
        loop {
            let mut line = String::new();
            let n = stream.read_line(&mut line).map_err(|e| e.to_string())?;
            if n == 0 || line == "\r\n" {
                break;
            }
            if let Some(v) = line.strip_prefix("Sec-WebSocket-Accept: ") {
                accept = Some(v.trim().to_owned());
            }
        }
        if !status.starts_with("HTTP/1.1 101") {
            return Err(status);
        }
        assert_eq!(accept.as_deref(), Some(accept_key(KEY).as_str()));
        Ok(Self { stream, raw })
    }

    pub fn frame(&mut self, opcode: u8, payload: &[u8]) {
        self.raw_frame(true, opcode, payload);
    }

    pub fn encode(fin: bool, opcode: u8, payload: &[u8], out: &mut Vec<u8>) {
        let mask = [1u8, 2, 3, 4];
        out.push(if fin { 0x80 } else { 0 } | opcode);
        let len = payload.len();
        if len < 126 {
            out.push(0x80 | len as u8);
        } else if len <= 0xFFFF {
            out.push(0x80 | 126);
            out.extend_from_slice(&(len as u16).to_be_bytes());
        } else {
            out.push(0x80 | 127);
            out.extend_from_slice(&(len as u64).to_be_bytes());
        }
        out.extend_from_slice(&mask);
        out.extend(payload.iter().enumerate().map(|(i, b)| b ^ mask[i % 4]));
    }

    pub fn raw_frame(&mut self, fin: bool, opcode: u8, payload: &[u8]) {
        let mut out = Vec::new();
        Self::encode(fin, opcode, payload, &mut out);
        self.write(&out);
    }

    /// Writes `bytes` in one call (over TLS: in as few records as fit).
    pub fn write(&mut self, bytes: &[u8]) {
        // The server may already have closed (a test of its close); that is not this
        // client's failure.
        let s = self.stream.get_mut();
        let _ = s.write_all(bytes).and_then(|()| s.flush());
    }

    pub fn read_frame(&mut self) -> std::io::Result<(u8, Vec<u8>)> {
        let mut h = [0u8; 2];
        self.stream.read_exact(&mut h)?;
        assert_eq!(h[1] & 0x80, 0, "server frames are not masked");
        let len = match h[1] & 0x7F {
            126 => {
                let mut b = [0u8; 2];
                self.stream.read_exact(&mut b)?;
                u64::from(u16::from_be_bytes(b))
            }
            127 => {
                let mut b = [0u8; 8];
                self.stream.read_exact(&mut b)?;
                u64::from_be_bytes(b)
            }
            n => u64::from(n),
        };
        let mut payload = vec![0; usize::try_from(len).unwrap()];
        self.stream.read_exact(&mut payload)?;
        Ok((h[0] & 0x0F, payload))
    }
}

impl Client for WsClient {
    fn send(&mut self, text: &str) {
        self.frame(0x1, text.as_bytes());
    }

    fn send_fragments(&mut self, parts: &[&str]) {
        for (i, part) in parts.iter().enumerate() {
            let opcode = if i == 0 { 0x1 } else { 0x0 };
            let fin = i + 1 == parts.len();
            self.raw_frame(fin, opcode, part.as_bytes());
        }
    }

    fn recv(&mut self, timeout: Duration) -> Option<ClientEvent> {
        self.raw.set_read_timeout(Some(timeout)).unwrap();
        loop {
            match self.read_frame() {
                Ok((0x1, payload)) => {
                    return Some(ClientEvent::Message(String::from_utf8(payload).unwrap()));
                }
                Ok((0x8, payload)) => {
                    let code =
                        (payload.len() >= 2).then(|| u16::from_be_bytes([payload[0], payload[1]]));
                    return Some(ClientEvent::Closed(code.and_then(close_reason)));
                }
                Ok((0xA, _)) => {}
                Ok((op, _)) => panic!("unexpected opcode {op}"),
                Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                    return None;
                }
                Err(_) => return Some(ClientEvent::Closed(None)),
            }
        }
    }

    fn close(&mut self) {
        self.frame(0x8, &1000u16.to_be_bytes());
        let _ = self.raw.shutdown(Shutdown::Write);
    }
}
