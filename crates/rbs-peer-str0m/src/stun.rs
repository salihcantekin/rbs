//! A small sans-IO STUN binding client (RFC 8489) for the server-reflexive address
//! and its keep-alive (ADR-0139 decisions 4 and 7). str0m does no gathering itself.
//!
//! Only plain binding requests to the user's STUN servers: no credentials, no
//! attributes. A response is accepted only for a transaction this client sent, from
//! the server it was sent to.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::{Duration, Instant};

/// RFC 8489 magic cookie.
const MAGIC: u32 = 0x2112_A442;
const HEADER_LEN: usize = 20;
const BINDING_REQUEST: u16 = 0x0001;
const BINDING_SUCCESS: u16 = 0x0101;
const ATTR_MAPPED_ADDRESS: u16 = 0x0001;
const ATTR_XOR_MAPPED_ADDRESS: u16 = 0x0020;
/// Time between retransmissions of one request.
pub const RETRY_INTERVAL: Duration = Duration::from_millis(500);
/// Sends of one request before the server counts as silent.
pub const MAX_SENDS: u32 = 5;
/// How often the binding is refreshed while a code is pending (ADR-0139 decision 4).
pub const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(20);

/// A STUN transaction id.
pub type TransactionId = [u8; 12];

/// A binding request, ready to send.
pub fn binding_request(id: &TransactionId) -> [u8; HEADER_LEN] {
    let mut out = [0u8; HEADER_LEN];
    out[0..2].copy_from_slice(&BINDING_REQUEST.to_be_bytes());
    // Length 0: no attributes.
    out[4..8].copy_from_slice(&MAGIC.to_be_bytes());
    out[8..20].copy_from_slice(id);
    out
}

/// Whether `bytes` looks like a STUN message (first two bits zero, the magic cookie),
/// so the socket loop can tell it from DTLS and RTP before handing it on.
pub fn is_stun(bytes: &[u8]) -> bool {
    bytes.len() >= HEADER_LEN && bytes[0] & 0xc0 == 0 && bytes[4..8] == MAGIC.to_be_bytes()
}

/// The mapped address of a binding success response for `id`; `None` for anything
/// else. Bounds-checked throughout: the bytes come from the network.
pub fn parse_binding_success(bytes: &[u8], id: &TransactionId) -> Option<SocketAddr> {
    if !is_stun(bytes) {
        return None;
    }
    let kind = u16::from_be_bytes([bytes[0], bytes[1]]);
    let len = usize::from(u16::from_be_bytes([bytes[2], bytes[3]]));
    if kind != BINDING_SUCCESS || &bytes[8..20] != id || bytes.len() < HEADER_LEN + len {
        return None;
    }
    let mut attrs = &bytes[HEADER_LEN..HEADER_LEN + len];
    let mut plain = None;
    while attrs.len() >= 4 {
        let ty = u16::from_be_bytes([attrs[0], attrs[1]]);
        let alen = usize::from(u16::from_be_bytes([attrs[2], attrs[3]]));
        let value = attrs.get(4..4 + alen)?;
        match ty {
            ATTR_XOR_MAPPED_ADDRESS => return address(value, Some(id)),
            ATTR_MAPPED_ADDRESS => plain = address(value, None),
            _ => {}
        }
        // Attributes are padded to 4 bytes.
        let next = (4 + alen).div_ceil(4) * 4;
        attrs = attrs.get(next..).unwrap_or(&[]);
    }
    plain
}

/// Reads a (XOR-)MAPPED-ADDRESS value; `xor` carries the transaction id for IPv6.
fn address(value: &[u8], xor: Option<&TransactionId>) -> Option<SocketAddr> {
    let family = *value.get(1)?;
    let mut port = u16::from_be_bytes([*value.get(2)?, *value.get(3)?]);
    let cookie = MAGIC.to_be_bytes();
    if xor.is_some() {
        port ^= u16::from_be_bytes([cookie[0], cookie[1]]);
    }
    let ip = match family {
        0x01 => {
            let mut b: [u8; 4] = value.get(4..8)?.try_into().ok()?;
            if xor.is_some() {
                for (byte, c) in b.iter_mut().zip(cookie) {
                    *byte ^= c;
                }
            }
            IpAddr::V4(Ipv4Addr::from(b))
        }
        0x02 => {
            let mut b: [u8; 16] = value.get(4..20)?.try_into().ok()?;
            if let Some(id) = xor {
                let key: Vec<u8> = cookie.iter().chain(id.iter()).copied().collect();
                for (byte, k) in b.iter_mut().zip(key) {
                    *byte ^= k;
                }
            }
            IpAddr::V6(Ipv6Addr::from(b))
        }
        _ => return None,
    };
    Some(SocketAddr::new(ip, port))
}

/// One request in flight.
#[derive(Debug, Clone)]
struct Pending {
    server: SocketAddr,
    id: TransactionId,
    sends: u32,
    next_send: Instant,
}

/// Finds the server-reflexive address through the user's STUN servers and keeps its
/// mapping open.
#[derive(Debug)]
pub struct Gatherer {
    servers: Vec<SocketAddr>,
    pending: Vec<Pending>,
    reflexive: Option<(SocketAddr, SocketAddr)>,
    next_keepalive: Option<Instant>,
    keepalive: bool,
}

/// A datagram the client wants sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Send {
    /// Where to.
    pub to: SocketAddr,
    /// The request.
    pub bytes: [u8; HEADER_LEN],
}

impl Gatherer {
    /// Starts one request per server at `now`; `ids` gives fresh transaction ids.
    pub fn start(
        servers: Vec<SocketAddr>,
        now: Instant,
        mut ids: impl FnMut() -> TransactionId,
    ) -> Self {
        let pending = servers
            .iter()
            .map(|&server| Pending {
                server,
                id: ids(),
                sends: 0,
                next_send: now,
            })
            .collect();
        Self {
            servers,
            pending,
            reflexive: None,
            next_keepalive: None,
            keepalive: false,
        }
    }

    /// Whether every server answered or went silent.
    pub fn done(&self) -> bool {
        self.reflexive.is_some() || self.pending.iter().all(|p| p.sends >= MAX_SENDS)
    }

    /// The server-reflexive address and the server that reported it.
    pub fn reflexive(&self) -> Option<SocketAddr> {
        self.reflexive.map(|(addr, _)| addr)
    }

    /// Refreshes the binding every [`KEEPALIVE_INTERVAL`] from now on (while a code is
    /// pending), or stops.
    pub fn set_keepalive(&mut self, on: bool, now: Instant) {
        self.keepalive = on;
        self.next_keepalive = on.then_some(now + KEEPALIVE_INTERVAL);
    }

    /// Takes a datagram from `from`. `true` when it was a response to this client.
    pub fn handle(&mut self, from: SocketAddr, bytes: &[u8]) -> bool {
        let Some(i) = self.pending.iter().position(|p| p.server == from) else {
            return false;
        };
        let Some(mapped) = parse_binding_success(bytes, &self.pending[i].id) else {
            return false;
        };
        self.pending.swap_remove(i);
        if self.reflexive.is_none() {
            self.reflexive = Some((mapped, from));
        }
        true
    }

    /// The next datagram due at `now`, if any.
    pub fn poll_send(
        &mut self,
        now: Instant,
        mut ids: impl FnMut() -> TransactionId,
    ) -> Option<Send> {
        if self.keepalive
            && let Some(at) = self.next_keepalive
            && now >= at
        {
            self.next_keepalive = Some(now + KEEPALIVE_INTERVAL);
            let server = self
                .reflexive
                .map(|(_, s)| s)
                .or(self.servers.first().copied())?;
            let id = ids();
            self.pending.retain(|p| p.server != server);
            self.pending.push(Pending {
                server,
                id,
                sends: 1,
                next_send: now + RETRY_INTERVAL,
            });
            return Some(Send {
                to: server,
                bytes: binding_request(&id),
            });
        }
        let gathering = self.reflexive.is_none();
        let p = self
            .pending
            .iter_mut()
            .find(|p| p.sends < MAX_SENDS && now >= p.next_send && (gathering || p.sends > 0))?;
        p.sends += 1;
        p.next_send = now + RETRY_INTERVAL;
        Some(Send {
            to: p.server,
            bytes: binding_request(&p.id),
        })
    }

    /// When [`Gatherer::poll_send`] next has something to send.
    pub fn next_timeout(&self) -> Option<Instant> {
        let retries = self
            .pending
            .iter()
            .filter(|p| p.sends < MAX_SENDS)
            .map(|p| p.next_send)
            .min();
        match (retries, self.next_keepalive.filter(|_| self.keepalive)) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    /// A binding success with an XOR-MAPPED-ADDRESS, as a STUN server writes it.
    pub(crate) fn success(id: &TransactionId, mapped: SocketAddr) -> Vec<u8> {
        let mut value = vec![0u8, 0, 0, 0];
        let cookie = MAGIC.to_be_bytes();
        let port = mapped.port() ^ u16::from_be_bytes([cookie[0], cookie[1]]);
        value[2..4].copy_from_slice(&port.to_be_bytes());
        match mapped.ip() {
            IpAddr::V4(ip) => {
                value[1] = 1;
                value.extend(ip.octets().iter().zip(cookie).map(|(b, c)| b ^ c));
            }
            IpAddr::V6(ip) => {
                value[1] = 2;
                let key: Vec<u8> = cookie.iter().chain(id.iter()).copied().collect();
                value.extend(ip.octets().iter().zip(key).map(|(b, k)| b ^ k));
            }
        }
        let mut out = Vec::new();
        out.extend_from_slice(&BINDING_SUCCESS.to_be_bytes());
        out.extend_from_slice(&((4 + value.len()) as u16).to_be_bytes());
        out.extend_from_slice(&cookie);
        out.extend_from_slice(id);
        out.extend_from_slice(&ATTR_XOR_MAPPED_ADDRESS.to_be_bytes());
        out.extend_from_slice(&(value.len() as u16).to_be_bytes());
        out.extend_from_slice(&value);
        out
    }

    /// The transaction id of a request.
    pub(crate) fn request_id(bytes: &[u8]) -> Option<TransactionId> {
        (is_stun(bytes) && bytes[0..2] == BINDING_REQUEST.to_be_bytes())
            .then(|| bytes[8..20].try_into().ok())
            .flatten()
    }

    fn counter() -> impl FnMut() -> TransactionId {
        let mut n = 0u8;
        move || {
            n += 1;
            [n; 12]
        }
    }

    #[test]
    fn mapped_addresses_parse_for_both_families() {
        let id = [7; 12];
        for addr in ["203.0.113.9:40000", "[2001:db8::5]:3478"] {
            let addr: SocketAddr = addr.parse().unwrap();
            assert_eq!(parse_binding_success(&success(&id, addr), &id), Some(addr));
        }
    }

    #[test]
    fn foreign_or_broken_responses_are_ignored() {
        let id = [7; 12];
        let addr: SocketAddr = "203.0.113.9:40000".parse().unwrap();
        let good = success(&id, addr);
        assert_eq!(
            parse_binding_success(&good, &[8; 12]),
            None,
            "other transaction"
        );
        for cut in 0..good.len() {
            assert_eq!(
                parse_binding_success(&good[..cut], &id),
                None,
                "cut at {cut}"
            );
        }
        let mut long_attr = good.clone();
        long_attr[23] = 0xff; // attribute length past the end
        assert_eq!(parse_binding_success(&long_attr, &id), None);
        assert!(!is_stun(&[0x80; 40]), "RTP is not STUN");
    }

    #[test]
    fn gathering_finds_the_reflexive_address_and_retries_silent_servers() {
        let t0 = Instant::now();
        let a: SocketAddr = "198.51.100.1:3478".parse().unwrap();
        let b: SocketAddr = "198.51.100.2:3478".parse().unwrap();
        let mut ids = counter();
        let mut g = Gatherer::start(vec![a, b], t0, &mut ids);
        let first = g.poll_send(t0, &mut ids).unwrap();
        let second = g.poll_send(t0, &mut ids).unwrap();
        assert_eq!((first.to, second.to), (a, b));
        assert!(g.poll_send(t0, &mut ids).is_none());
        assert_eq!(g.next_timeout(), Some(t0 + RETRY_INTERVAL));
        let id = request_id(&second.bytes).unwrap();
        let mapped: SocketAddr = "203.0.113.9:40000".parse().unwrap();
        assert!(
            !g.handle(a, &success(&id, mapped)),
            "right id, wrong server"
        );
        assert!(g.handle(b, &success(&id, mapped)));
        assert_eq!(g.reflexive(), Some(mapped));
        assert!(g.done());
    }

    #[test]
    fn silent_servers_end_gathering_after_the_last_retry() {
        let t0 = Instant::now();
        let a: SocketAddr = "198.51.100.1:3478".parse().unwrap();
        let mut ids = counter();
        let mut g = Gatherer::start(vec![a], t0, &mut ids);
        let mut sends = 0;
        let mut now = t0;
        while !g.done() {
            while g.poll_send(now, &mut ids).is_some() {
                sends += 1;
            }
            now += RETRY_INTERVAL;
        }
        assert_eq!(sends, MAX_SENDS);
        assert_eq!(g.reflexive(), None);
    }

    #[test]
    fn the_keepalive_refreshes_every_twenty_seconds() {
        let t0 = Instant::now();
        let a: SocketAddr = "198.51.100.1:3478".parse().unwrap();
        let mut ids = counter();
        let mut g = Gatherer::start(vec![a], t0, &mut ids);
        let req = g.poll_send(t0, &mut ids).unwrap();
        let mapped: SocketAddr = "203.0.113.9:40000".parse().unwrap();
        assert!(g.handle(a, &success(&request_id(&req.bytes).unwrap(), mapped)));
        g.set_keepalive(true, t0);
        assert!(
            g.poll_send(t0 + KEEPALIVE_INTERVAL - Duration::from_millis(1), &mut ids)
                .is_none()
        );
        let refresh = g.poll_send(t0 + KEEPALIVE_INTERVAL, &mut ids).unwrap();
        assert_eq!(refresh.to, a);
        // The refresh is answered; with the keep-alive off nothing more is sent.
        assert!(g.handle(a, &success(&request_id(&refresh.bytes).unwrap(), mapped)));
        g.set_keepalive(false, t0);
        assert_eq!(g.poll_send(t0 + KEEPALIVE_INTERVAL * 3, &mut ids), None);
        assert_eq!(g.next_timeout(), None);
    }
}
