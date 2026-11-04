//! `Origin` and `Host` checks before the WebSocket upgrade (ADR-0044 decision 11).

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
    assert_eq!(p.check(None, Some("localhost:4455")), Ok(()));
    assert_eq!(
        p.check(Some("https://PANEL.example"), Some("localhost")),
        Ok(())
    );
    assert!(matches!(
        p.check(Some("null"), Some("localhost")),
        Err(UpgradeRefusal::Origin(_))
    ));
    assert!(matches!(
        p.check(Some("https://evil.example"), Some("localhost")),
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
        assert_eq!(p.check(None, Some(ok)), Ok(()), "{ok}");
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
        assert!(p.check(None, Some(bad)).is_err(), "{bad}");
    }
    assert_eq!(p.check(None, None), Err(UpgradeRefusal::MissingHost));
}
