//! `Origin` and `Host` checks before the WebSocket upgrade (ADR-0044 decision 11).

#![allow(clippy::unwrap_used)] // Tests fail by panicking.

use rbs_api::{UpgradePolicy, UpgradeRefusal};

fn policy() -> UpgradePolicy {
    UpgradePolicy {
        allowed_origins: vec!["https://panel.example".into(), "null".into()],
        host_names: vec!["studio.local".into()],
        bound_addresses: vec![
            "192.168.1.5"
                .parse()
                .unwrap_or(std::net::IpAddr::from([0, 0, 0, 0])),
        ],
    }
}

#[test]
fn origins_must_be_on_the_allow_list_and_null_never_is() {
    let p = policy();
    assert_eq!(p.check(None, Some("localhost:4455"), None), Ok(()));
    assert_eq!(
        p.check(Some("https://PANEL.example"), Some("localhost"), None),
        Ok(())
    );
    assert!(matches!(
        p.check(Some("null"), Some("localhost"), None),
        Err(UpgradeRefusal::Origin(_))
    ));
    assert!(matches!(
        p.check(Some("https://evil.example"), Some("localhost"), None),
        Err(UpgradeRefusal::Origin(_))
    ));
}

#[test]
fn host_must_be_loopback_configured_or_bound() {
    let p = policy();
    for ok in [
        "localhost",
        "LOCALHOST:4455",
        "127.0.0.1:4455",
        "127.1.2.3",
        "[::1]:4455",
        "studio.local:4455",
        "studio.local.",
        "192.168.1.5:4455",
    ] {
        assert_eq!(p.check(None, Some(ok), None), Ok(()), "{ok}");
    }
    // DNS rebinding: an attacker's name resolving to 127.0.0.1 is refused.
    for bad in [
        "attacker.example",
        "10.0.0.1",
        "::1",
        "[::1",
        "localhost:http",
        "",
    ] {
        assert!(p.check(None, Some(bad), None).is_err(), "{bad}");
    }
    assert_eq!(p.check(None, None, None), Err(UpgradeRefusal::MissingHost));
}

#[test]
fn host_may_name_the_accepting_address_but_never_an_unspecified_one() {
    let p = UpgradePolicy {
        bound_addresses: vec!["0.0.0.0".parse().unwrap(), "::".parse().unwrap()],
        ..UpgradePolicy::default()
    };
    let local = Some("192.168.1.7".parse().unwrap());
    assert_eq!(p.check(None, Some("192.168.1.7:4456"), local), Ok(()));
    // A dual-stack socket reports IPv4 clients' local address as IPv4-mapped IPv6.
    let mapped = Some("::ffff:192.168.1.7".parse().unwrap());
    assert_eq!(p.check(None, Some("192.168.1.7:4456"), mapped), Ok(()));
    assert!(p.check(None, Some("192.168.1.8"), local).is_err());
    for bad in ["0.0.0.0:4456", "[::]:4456"] {
        assert!(p.check(None, Some(bad), local).is_err(), "{bad}");
        assert!(
            p.check(None, Some(bad), Some("0.0.0.0".parse().unwrap()))
                .is_err()
        );
    }
}
