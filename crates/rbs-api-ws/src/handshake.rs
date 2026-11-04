//! The HTTP upgrade (RFC 6455 section 4.2): parsed and checked completely, including
//! `Origin` and `Host` (ADR-0044 decision 11), before the server answers `101`.

use rbs_api::{UpgradePolicy, UpgradeRefusal};
use sha1::{Digest, Sha1};

/// The subprotocol of protocol version 1.
pub const SUBPROTOCOL: &str = "rbs.v1";

/// The largest upgrade request read.
pub(crate) const MAX_REQUEST: usize = 8 * 1024;

/// An accepted upgrade request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Upgrade {
    /// `Sec-WebSocket-Key`.
    pub key: String,
    /// The client offered `rbs.v1`.
    pub subprotocol: bool,
    /// The `Origin` header.
    pub origin: Option<String>,
    /// The `Host` header.
    pub host: Option<String>,
    /// Bytes after the request (frames the client sent at once).
    pub rest: usize,
}

/// Why an upgrade is refused, as the HTTP status to answer with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Refusal {
    /// 400: not a valid WebSocket upgrade.
    BadRequest(&'static str),
    /// 426: another WebSocket version.
    Version,
    /// 403: `Origin` or `Host` refused.
    Forbidden(UpgradeRefusal),
    /// 431: the request is too long.
    TooLarge,
}

impl Refusal {
    /// The response to send before closing.
    pub(crate) fn response(&self) -> &'static [u8] {
        match self {
            Refusal::BadRequest(_) => {
                b"HTTP/1.1 400 Bad Request\r\nConnection: close\r\nContent-Length: 0\r\n\r\n"
            }
            Refusal::Version => {
                b"HTTP/1.1 426 Upgrade Required\r\nSec-WebSocket-Version: 13\r\nConnection: close\r\nContent-Length: 0\r\n\r\n"
            }
            Refusal::Forbidden(_) => {
                b"HTTP/1.1 403 Forbidden\r\nConnection: close\r\nContent-Length: 0\r\n\r\n"
            }
            Refusal::TooLarge => {
                b"HTTP/1.1 431 Request Header Fields Too Large\r\nConnection: close\r\nContent-Length: 0\r\n\r\n"
            }
        }
    }
}

/// The answer when the connection cap is reached or the server stops.
pub(crate) const UNAVAILABLE: &[u8] =
    b"HTTP/1.1 503 Service Unavailable\r\nConnection: close\r\nContent-Length: 0\r\n\r\n";

/// Reads the upgrade request at the start of `buf`: `Ok(None)` while it is incomplete.
pub(crate) fn parse(buf: &[u8], policy: &UpgradePolicy) -> Result<Option<Upgrade>, Refusal> {
    let mut headers = [httparse::EMPTY_HEADER; 32];
    let mut req = httparse::Request::new(&mut headers);
    let rest = match req.parse(buf) {
        Ok(httparse::Status::Complete(n)) => n,
        Ok(httparse::Status::Partial) if buf.len() >= MAX_REQUEST => {
            return Err(Refusal::TooLarge);
        }
        Ok(httparse::Status::Partial) => return Ok(None),
        Err(httparse::Error::TooManyHeaders) => return Err(Refusal::TooLarge),
        Err(_) => return Err(Refusal::BadRequest("malformed request")),
    };
    if req.method != Some("GET") || req.version != Some(1) {
        return Err(Refusal::BadRequest("not GET over HTTP/1.1"));
    }
    let mut upgrade = false;
    let mut connection = false;
    let mut version = None;
    let mut key = None;
    let mut origin = None;
    let mut host = None;
    let mut offered = false;
    let mut subprotocol = false;
    for h in req.headers.iter() {
        let value =
            std::str::from_utf8(h.value).map_err(|_| Refusal::BadRequest("header not UTF-8"))?;
        let name = h.name;
        if name.eq_ignore_ascii_case("upgrade") {
            upgrade |= has_token(value, "websocket");
        } else if name.eq_ignore_ascii_case("connection") {
            connection |= has_token(value, "upgrade");
        } else if name.eq_ignore_ascii_case("sec-websocket-version") {
            version = Some(value.trim());
        } else if name.eq_ignore_ascii_case("sec-websocket-key") {
            once(&mut key, value.trim())?;
        } else if name.eq_ignore_ascii_case("origin") {
            once(&mut origin, value.trim())?;
        } else if name.eq_ignore_ascii_case("host") {
            once(&mut host, value.trim())?;
        } else if name.eq_ignore_ascii_case("sec-websocket-protocol") {
            offered = true;
            subprotocol |= has_token(value, SUBPROTOCOL);
        }
    }
    if !upgrade || !connection {
        return Err(Refusal::BadRequest("not a WebSocket upgrade"));
    }
    if version != Some("13") {
        return Err(Refusal::Version);
    }
    let key = key.ok_or(Refusal::BadRequest("no Sec-WebSocket-Key"))?;
    if key.len() != 24 || !key.ends_with("==") {
        return Err(Refusal::BadRequest("Sec-WebSocket-Key is not 16 bytes"));
    }
    // A client that names subprotocols must name ours; one that names none gets the
    // protocol of version 1 without the header.
    if offered && !subprotocol {
        return Err(Refusal::BadRequest("rbs.v1 not offered"));
    }
    policy.check(origin, host).map_err(Refusal::Forbidden)?;
    Ok(Some(Upgrade {
        key: key.to_owned(),
        subprotocol,
        origin: origin.map(str::to_owned),
        host: host.map(str::to_owned),
        rest,
    }))
}

/// A header that may appear once: a second one makes the request ambiguous.
fn once<'a>(slot: &mut Option<&'a str>, value: &'a str) -> Result<(), Refusal> {
    if slot.replace(value).is_some() {
        return Err(Refusal::BadRequest("repeated header"));
    }
    Ok(())
}

fn has_token(value: &str, token: &str) -> bool {
    value
        .split(',')
        .any(|t| t.trim().eq_ignore_ascii_case(token))
}

/// The `101` answer.
pub(crate) fn accept_response(upgrade: &Upgrade) -> Vec<u8> {
    let mut out = format!(
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {}\r\n",
        accept_key(&upgrade.key)
    );
    if upgrade.subprotocol {
        out.push_str("Sec-WebSocket-Protocol: ");
        out.push_str(SUBPROTOCOL);
        out.push_str("\r\n");
    }
    out.push_str("\r\n");
    out.into_bytes()
}

/// `Sec-WebSocket-Accept` for a key (RFC 6455 section 4.2.2).
pub fn accept_key(key: &str) -> String {
    let mut h = Sha1::new();
    h.update(key.as_bytes());
    h.update(b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11");
    base64(&h.finalize())
}

fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = (u32::from(chunk[0]) << 16)
            | (u32::from(chunk.get(1).copied().unwrap_or(0)) << 8)
            | u32::from(chunk.get(2).copied().unwrap_or(0));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(char::from(ALPHABET[(n >> (18 - 6 * i)) as usize & 63]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used)] // Tests fail by panicking.
mod tests {
    use super::*;

    fn request(extra: &str) -> String {
        format!(
            "GET /chat HTTP/1.1\r\nHost: localhost:4455\r\nUpgrade: websocket\r\nConnection: keep-alive, Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n{extra}\r\n"
        )
    }

    #[test]
    fn the_rfc_key_gives_the_rfc_accept() {
        assert_eq!(
            accept_key("dGhlIHNhbXBsZSBub25jZQ=="),
            "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
        );
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
    }

    #[test]
    fn a_valid_upgrade_is_accepted_with_or_without_the_subprotocol() {
        let policy = UpgradePolicy::default();
        let plain = request("");
        let u = parse(plain.as_bytes(), &policy).unwrap().unwrap();
        assert!(!u.subprotocol);
        assert_eq!(u.rest, plain.len());
        assert_eq!(u.host.as_deref(), Some("localhost:4455"));
        let with = request("Sec-WebSocket-Protocol: other, rbs.v1\r\n");
        let u = parse(with.as_bytes(), &policy).unwrap().unwrap();
        assert!(u.subprotocol);
        let answer = String::from_utf8(accept_response(&u)).unwrap();
        assert!(
            answer.contains("Sec-WebSocket-Protocol: rbs.v1\r\n"),
            "{answer}"
        );
        assert!(answer.contains("s3pPLMBiTxaQ9kYGzzhZRbK+xOo="), "{answer}");
    }

    #[test]
    fn an_incomplete_request_waits_and_a_huge_one_is_refused() {
        let policy = UpgradePolicy::default();
        let r = request("");
        assert_eq!(parse(&r.as_bytes()[..20], &policy), Ok(None));
        let huge = format!("GET / HTTP/1.1\r\nX: {}", "a".repeat(MAX_REQUEST));
        assert_eq!(parse(huge.as_bytes(), &policy), Err(Refusal::TooLarge));
    }

    #[test]
    fn invalid_upgrades_are_refused() {
        let policy = UpgradePolicy::default();
        let cases = [
            request("Sec-WebSocket-Protocol: other\r\n"),
            request("Origin: null\r\n"),
            request("Host: evil.example\r\n"),
            request("").replace("Version: 13", "Version: 8"),
            request("").replace("Upgrade: websocket", "Upgrade: h2c"),
            request("").replace("POST", "GET").replace("GET", "POST"),
            request("").replace("dGhlIHNhbXBsZSBub25jZQ==", "short"),
        ];
        for c in &cases {
            assert!(parse(c.as_bytes(), &policy).is_err(), "{c}");
        }
        assert_eq!(parse(cases[3].as_bytes(), &policy), Err(Refusal::Version));
    }
}
