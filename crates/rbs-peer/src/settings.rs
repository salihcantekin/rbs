//! Pairing settings and their bounds (docs/specs/peer-port.md, "Settings and bounds").
//!
//! The port validates; it never clamps. An adapter sees only validated settings.

use std::time::Duration;

use crate::error::SettingsError;

/// Default invite lifetime.
pub const DEFAULT_INVITE_LIFETIME: Duration = Duration::from_secs(10 * 60);
/// Longest invite lifetime.
pub const MAX_INVITE_LIFETIME: Duration = Duration::from_secs(30 * 60);
/// Default answer lifetime.
pub const DEFAULT_ANSWER_LIFETIME: Duration = Duration::from_secs(3 * 60);
/// Longest answer lifetime.
pub const MAX_ANSWER_LIFETIME: Duration = Duration::from_secs(10 * 60);
/// Shortest invite or answer lifetime.
pub const MIN_LIFETIME: Duration = Duration::from_secs(60);
/// Default candidate gathering timeout.
pub const DEFAULT_GATHERING_TIMEOUT: Duration = Duration::from_secs(3);
/// Shortest candidate gathering timeout.
pub const MIN_GATHERING_TIMEOUT: Duration = Duration::from_millis(500);
/// Longest candidate gathering timeout.
pub const MAX_GATHERING_TIMEOUT: Duration = Duration::from_secs(10);
/// Most STUN servers in the list.
pub const MAX_STUN_SERVERS: usize = 8;
/// Longest STUN server entry, in bytes.
pub const MAX_STUN_SERVER_LEN: usize = 253;

/// Settings both sides share.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct NetworkSettings {
    /// How long candidate gathering may take before the code is shown.
    pub gathering_timeout: Duration,
    /// STUN servers, `host:port` or `stun:host:port`. Empty: local network only.
    pub stun_servers: Vec<String>,
    /// Leave private host addresses out of the code (pairings across the internet).
    pub omit_private_addresses: bool,
    /// Compare the check phrase before media is used on this side.
    pub check_phrase: bool,
}

impl NetworkSettings {
    /// Settings with the given STUN list and every other value at its default.
    pub fn with_stun_servers(stun_servers: Vec<String>) -> Self {
        Self {
            gathering_timeout: DEFAULT_GATHERING_TIMEOUT,
            stun_servers,
            omit_private_addresses: false,
            check_phrase: true,
        }
    }

    fn validate(&self) -> Result<(), SettingsError> {
        if !(MIN_GATHERING_TIMEOUT..=MAX_GATHERING_TIMEOUT).contains(&self.gathering_timeout) {
            return Err(SettingsError::GatheringTimeout);
        }
        if self.stun_servers.len() > MAX_STUN_SERVERS {
            return Err(SettingsError::TooManyStunServers);
        }
        self.stun_servers
            .iter()
            .try_for_each(|s| validate_stun_server(s))
    }
}

/// Settings for *Share* (the inviting side).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct InviteSettings {
    /// How long the invite stays valid.
    pub lifetime: Duration,
    /// Network settings.
    pub network: NetworkSettings,
}

impl InviteSettings {
    /// An invite with the default lifetime.
    pub fn new(network: NetworkSettings) -> Self {
        Self {
            lifetime: DEFAULT_INVITE_LIFETIME,
            network,
        }
    }

    /// Checks every bound.
    pub fn validate(&self) -> Result<(), SettingsError> {
        if !(MIN_LIFETIME..=MAX_INVITE_LIFETIME).contains(&self.lifetime) {
            return Err(SettingsError::Lifetime);
        }
        self.network.validate()
    }
}

/// Settings for *Add source › Guest* (the answering side).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct AcceptSettings {
    /// How long the answer stays valid.
    pub lifetime: Duration,
    /// Network settings.
    pub network: NetworkSettings,
}

impl AcceptSettings {
    /// An answer with the default lifetime.
    pub fn new(network: NetworkSettings) -> Self {
        Self {
            lifetime: DEFAULT_ANSWER_LIFETIME,
            network,
        }
    }

    /// Checks every bound.
    pub fn validate(&self) -> Result<(), SettingsError> {
        if !(MIN_LIFETIME..=MAX_ANSWER_LIFETIME).contains(&self.lifetime) {
            return Err(SettingsError::Lifetime);
        }
        self.network.validate()
    }
}

/// `host:port` or `stun:host:port`; an IPv6 host is in brackets.
fn validate_stun_server(entry: &str) -> Result<(), SettingsError> {
    if entry.is_empty() || entry.len() > MAX_STUN_SERVER_LEN {
        return Err(SettingsError::StunServer);
    }
    let rest = entry.strip_prefix("stun:").unwrap_or(entry);
    let (host, port) = rest.rsplit_once(':').ok_or(SettingsError::StunServer)?;
    let port_ok = !port.is_empty()
        && port.bytes().all(|b| b.is_ascii_digit())
        && port.parse::<u16>().is_ok_and(|p| p != 0);
    let host_ok = if let Some(inner) = host.strip_prefix('[') {
        inner.strip_suffix(']').is_some_and(|ip| {
            !ip.is_empty()
                && ip
                    .bytes()
                    .all(|b| b.is_ascii_hexdigit() || b == b':' || b == b'.')
        })
    } else {
        !host.is_empty()
            && host
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
    };
    if port_ok && host_ok {
        Ok(())
    } else {
        Err(SettingsError::StunServer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn network() -> NetworkSettings {
        NetworkSettings::with_stun_servers(vec!["stun.example.org:3478".to_owned()])
    }

    #[test]
    fn defaults_are_valid() {
        assert_eq!(InviteSettings::new(network()).validate(), Ok(()));
        assert_eq!(AcceptSettings::new(network()).validate(), Ok(()));
        let local_only = NetworkSettings::with_stun_servers(Vec::new());
        assert_eq!(InviteSettings::new(local_only).validate(), Ok(()));
    }

    #[test]
    fn lifetimes_are_bounded_not_clamped() {
        let mut invite = InviteSettings::new(network());
        invite.lifetime = MAX_INVITE_LIFETIME;
        assert_eq!(invite.validate(), Ok(()));
        invite.lifetime = MAX_INVITE_LIFETIME + Duration::from_secs(1);
        assert_eq!(invite.validate(), Err(SettingsError::Lifetime));
        invite.lifetime = Duration::from_secs(59);
        assert_eq!(invite.validate(), Err(SettingsError::Lifetime));

        let mut answer = AcceptSettings::new(network());
        answer.lifetime = MAX_ANSWER_LIFETIME;
        assert_eq!(answer.validate(), Ok(()));
        answer.lifetime = MAX_ANSWER_LIFETIME + Duration::from_secs(1);
        assert_eq!(answer.validate(), Err(SettingsError::Lifetime));
    }

    #[test]
    fn gathering_timeout_is_bounded() {
        let mut net = network();
        net.gathering_timeout = Duration::from_millis(499);
        assert_eq!(
            InviteSettings::new(net.clone()).validate(),
            Err(SettingsError::GatheringTimeout)
        );
        net.gathering_timeout = MAX_GATHERING_TIMEOUT + Duration::from_millis(1);
        assert_eq!(
            InviteSettings::new(net).validate(),
            Err(SettingsError::GatheringTimeout)
        );
    }

    #[test]
    fn stun_entries_are_checked() {
        for good in [
            "stun.l.example.com:19302",
            "stun:1.2.3.4:3478",
            "[2001:db8::1]:3478",
            "stun:[::1]:1",
        ] {
            assert_eq!(validate_stun_server(good), Ok(()), "{good}");
        }
        for bad in [
            "",
            "stun.example.org",
            "stun.example.org:",
            "stun.example.org:0",
            "stun.example.org:70000",
            ":3478",
            "turn:relay:3478:x",
            "host name:3478",
            "[]:3478",
            "[::1:3478",
            "stun.example.org:+1",
        ] {
            assert_eq!(
                validate_stun_server(bad),
                Err(SettingsError::StunServer),
                "{bad}"
            );
        }
        let long = format!("{}:3478", "a".repeat(MAX_STUN_SERVER_LEN));
        assert_eq!(validate_stun_server(&long), Err(SettingsError::StunServer));
    }

    #[test]
    fn the_stun_list_is_bounded() {
        let net = NetworkSettings::with_stun_servers(vec![
            "a.example:1".to_owned();
            MAX_STUN_SERVERS + 1
        ]);
        assert_eq!(
            InviteSettings::new(net).validate(),
            Err(SettingsError::TooManyStunServers)
        );
    }
}
