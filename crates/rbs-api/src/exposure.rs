//! Checks a WebSocket adapter runs before the upgrade completes (ADR-0044 decision 11):
//! `Origin` against the user's allow list, `Host` against the names and addresses the
//! server answers to. Shared here so every adapter applies the same rules (cross-site
//! WebSocket hijacking, DNS rebinding).

use std::net::IpAddr;

/// Why an upgrade is refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpgradeRefusal {
    /// The `Origin` header is not on the allow list (`null` never is).
    Origin(String),
    /// No `Host` header.
    MissingHost,
    /// The `Host` header names something this server does not answer to.
    Host(String),
}

/// What the server accepts in `Origin` and `Host`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UpgradePolicy {
    /// Origins the user allowed (`https://panel.example`), compared without regard to
    /// ASCII case. `null` is ignored here: it can never be allowed.
    pub allowed_origins: Vec<String>,
    /// Host names the user configured for network access (`studio.local`).
    pub host_names: Vec<String>,
    /// Addresses of the interfaces the server is bound to.
    pub bound_addresses: Vec<IpAddr>,
}

impl UpgradePolicy {
    /// Checks the request's `Origin` and `Host` headers.
    pub fn check(&self, origin: Option<&str>, host: Option<&str>) -> Result<(), UpgradeRefusal> {
        if let Some(origin) = origin {
            let allowed = !origin.trim().eq_ignore_ascii_case("null")
                && self
                    .allowed_origins
                    .iter()
                    .any(|o| !o.eq_ignore_ascii_case("null") && o.eq_ignore_ascii_case(origin));
            if !allowed {
                return Err(UpgradeRefusal::Origin(origin.to_owned()));
            }
        }
        let host = host.ok_or(UpgradeRefusal::MissingHost)?;
        let name = strip_port(host).ok_or_else(|| UpgradeRefusal::Host(host.to_owned()))?;
        if name.eq_ignore_ascii_case("localhost") {
            return Ok(());
        }
        if let Ok(ip) = name.parse::<IpAddr>() {
            if ip.is_loopback() || self.bound_addresses.contains(&ip) {
                return Ok(());
            }
            return Err(UpgradeRefusal::Host(host.to_owned()));
        }
        if self.host_names.iter().any(|n| {
            n.trim_end_matches('.')
                .eq_ignore_ascii_case(name.trim_end_matches('.'))
        }) {
            return Ok(());
        }
        Err(UpgradeRefusal::Host(host.to_owned()))
    }
}

/// The name or address of a `Host` header without its port: `[::1]:4455` is `::1`,
/// `localhost:4455` is `localhost`. `None` for a malformed header.
fn strip_port(host: &str) -> Option<&str> {
    let host = host.trim();
    if let Some(rest) = host.strip_prefix('[') {
        let (addr, after) = rest.split_once(']')?;
        return (after.is_empty() || after.starts_with(':')).then_some(addr);
    }
    match host.rsplit_once(':') {
        // More than one colon without brackets: not a valid Host header.
        Some((name, _)) if name.contains(':') => None,
        Some((name, port)) if port.bytes().all(|b| b.is_ascii_digit()) => Some(name),
        Some(_) => None,
        None => Some(host),
    }
    .filter(|n| !n.is_empty())
}
