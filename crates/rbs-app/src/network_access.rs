//! The TLS identity for network access (ADR-0046 decision 4): the key and certificate
//! RBS made, renewed with the same key when fewer than 30 days are left, or the user's
//! own. Made only when the user first turns network access on, or asks for a new key;
//! a stored key that went missing is never replaced silently, so a changed key pin is
//! always something the user did.
//!
//! The settings file and PEM files are `rbs-remote-settings`; making and checking them
//! is `rbs_api_ws::tls`. The caller holds the settings lock while this changes them.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, UdpSocket};
use std::path::{Path, PathBuf};

use rbs_api_ws::TlsIdentity;
use rbs_api_ws::tls;
use rbs_remote_settings::{self as settings, Pem, RemoteProfile};

/// A certificate is renewed when it has fewer than this many seconds left.
pub const RENEW_BEFORE: i64 = 30 * 24 * 60 * 60;

/// Where the identity came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// The key RBS made; `renewed` when its certificate was renewed just now (the
    /// certificate fingerprint changed, the key pin did not).
    Generated {
        /// The certificate was renewed now.
        renewed: bool,
        /// It was made now: the first time network access was turned on.
        created: bool,
    },
    /// The user's own certificate file.
    Own(PathBuf),
}

/// The identity to serve with, and where it came from.
#[derive(Debug, Clone)]
pub struct Prepared {
    /// What the server presents.
    pub identity: TlsIdentity,
    /// Where it came from.
    pub source: Source,
}

/// The names and addresses a new certificate is made for: `localhost`, this
/// computer's name and `<name>.local`, loopback, and the address of the interface
/// that reaches the network now. Clients pin the key, so a list that is out of date
/// later (DHCP) does not matter to them.
pub fn certificate_names() -> (Vec<String>, Vec<IpAddr>) {
    let mut names = vec!["localhost".to_owned()];
    names.extend(host_names());
    let mut addresses = vec![
        IpAddr::V4(Ipv4Addr::LOCALHOST),
        IpAddr::V6(Ipv6Addr::LOCALHOST),
    ];
    addresses.extend(network_address());
    (names, addresses)
}

/// This computer's name and `<name>.local`, which clients on the network may use in
/// `Host`; empty when the system does not say.
pub fn host_names() -> Vec<String> {
    let Some(name) = computer_name() else {
        return Vec::new();
    };
    let name = name.trim().trim_end_matches(".local").to_ascii_lowercase();
    if name.is_empty() || name == "localhost" || !name.chars().all(valid_host_char) {
        return Vec::new();
    }
    vec![format!("{name}.local"), name]
}

fn valid_host_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '-' || c == '.'
}

fn computer_name() -> Option<String> {
    if cfg!(windows) {
        return std::env::var("COMPUTERNAME").ok();
    }
    // `hostname` exists on every Unix. Called when the server starts, not per frame.
    let out = std::process::Command::new("hostname").output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The address of the interface on the default route, without sending anything.
fn network_address() -> Option<IpAddr> {
    let s = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    s.connect((Ipv4Addr::new(198, 51, 100, 1), 9)).ok()?;
    let ip = s.local_addr().ok()?.ip();
    (!ip.is_loopback() && !ip.is_unspecified()).then_some(ip)
}

/// The identity for network access with `profile` (saved in `config`) at `now`
/// (seconds since the Unix epoch). Makes the key and certificate the first time, and
/// renews the certificate when it is about to expire; both save the files and
/// `profile` (`certificate_expires`). Refused, in words, when the files cannot be
/// used: then network access does not start and the user can make a new key.
pub fn prepare(config: &Path, profile: &mut RemoteProfile, now: i64) -> Result<Prepared, String> {
    if let Some(own) = &profile.own_certificate {
        let pem = settings::read_own(own)?;
        let identity = TlsIdentity::from_pem(&pem.certificate, &pem.key)
            .map_err(|why| format!("your certificate cannot be used: {why}"))?;
        return Ok(Prepared {
            identity,
            source: Source::Own(own.certificate.clone()),
        });
    }
    match settings::read_generated(config)? {
        None if profile.certificate_expires.is_none() => {
            new_key(config, profile, now)?;
            let mut p = prepare(config, profile, now)?;
            p.source = Source::Generated {
                renewed: false,
                created: true,
            };
            Ok(p)
        }
        None => Err(format!(
            "the key RBS made for access from other computers is missing from {}. Make a new key; every app on another computer must then check the new key pin",
            config.display()
        )),
        Some(pem) => {
            let mut identity = TlsIdentity::from_pem(&pem.certificate, &pem.key).map_err(|why| {
                format!("the stored key and certificate cannot be used ({why}). Make a new key; every app on another computer must then check the new key pin")
            })?;
            let renewed = profile
                .certificate_expires
                .is_none_or(|t| t - now < RENEW_BEFORE);
            if renewed {
                let (names, addresses) = certificate_names();
                let g = tls::renew(&pem.key, &names, &addresses, now)?;
                identity = TlsIdentity::from_pem(&g.cert_pem, &g.key_pem)?;
                save(config, profile, &g)?;
            }
            Ok(Prepared {
                identity,
                source: Source::Generated {
                    renewed,
                    created: false,
                },
            })
        }
    }
}

/// Makes a new key and certificate ("New key"): every client must pin again. Saves
/// the files and `profile`.
pub fn new_key(config: &Path, profile: &mut RemoteProfile, now: i64) -> Result<(), String> {
    let (names, addresses) = certificate_names();
    let g = tls::generate(&names, &addresses, now)?;
    save(config, profile, &g)
}

fn save(config: &Path, profile: &mut RemoteProfile, g: &tls::Generated) -> Result<(), String> {
    settings::save_generated(
        config,
        &Pem {
            certificate: g.cert_pem.clone(),
            key: g.key_pem.clone(),
        },
    )
    .map_err(|e| format!("the key and certificate could not be saved: {e}"))?;
    profile.certificate_expires = Some(g.not_after);
    settings::save(config, profile)
        .map_err(|e| format!("the remote control settings could not be saved: {e}"))
}

/// Seconds since the Unix epoch now.
pub fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.
mod tests {
    use super::*;

    const NOW: i64 = 1_800_000_000;
    const DAY: i64 = 24 * 60 * 60;

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("rbs-app-network-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn made_once_then_kept_then_renewed_with_the_same_pin() {
        let d = dir("lifecycle");
        let mut p = RemoteProfile::new();
        let first = prepare(&d, &mut p, NOW).unwrap();
        assert_eq!(
            first.source,
            Source::Generated {
                renewed: false,
                created: true
            }
        );
        let expires = p.certificate_expires.unwrap();
        assert!(expires > NOW + 800 * DAY);
        assert_eq!(
            settings::read(&d).unwrap().unwrap(),
            p,
            "the expiry is saved"
        );

        let again = prepare(&d, &mut p, NOW + DAY).unwrap();
        assert_eq!(again.identity, first.identity, "kept as it is");
        assert_eq!(
            again.source,
            Source::Generated {
                renewed: false,
                created: false
            }
        );

        let late = expires - 10 * DAY;
        let renewed = prepare(&d, &mut p, late).unwrap();
        assert_eq!(
            renewed.source,
            Source::Generated {
                renewed: true,
                created: false
            }
        );
        assert_eq!(renewed.identity.key_pin(), first.identity.key_pin());
        assert_ne!(renewed.identity.fingerprint(), first.identity.fingerprint());
        assert!(p.certificate_expires.unwrap() > late + 800 * DAY);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_missing_or_mismatched_key_is_never_replaced_silently() {
        let d = dir("missing");
        let mut p = RemoteProfile::new();
        let first = prepare(&d, &mut p, NOW).unwrap();
        std::fs::remove_file(settings::key_file(&d)).unwrap();
        assert!(prepare(&d, &mut p, NOW).unwrap_err().contains("missing"));
        std::fs::remove_file(settings::cert_file(&d)).unwrap();
        assert!(prepare(&d, &mut p, NOW).unwrap_err().contains("missing"));

        // A key from elsewhere: refused, not regenerated.
        let other = tls::generate(&["x".into()], &[], NOW).unwrap();
        settings::save_generated(
            &d,
            &Pem {
                certificate: other.cert_pem,
                key: tls::generate(&["y".into()], &[], NOW).unwrap().key_pem,
            },
        )
        .unwrap();
        assert!(prepare(&d, &mut p, NOW).unwrap_err().contains("new key"));

        new_key(&d, &mut p, NOW).unwrap();
        let fresh = prepare(&d, &mut p, NOW).unwrap();
        assert_ne!(fresh.identity.key_pin(), first.identity.key_pin());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn the_users_own_certificate_is_used_and_never_copied() {
        let d = dir("own");
        std::fs::create_dir_all(&d).unwrap();
        let g = tls::generate(&["studio.example".into()], &[], NOW).unwrap();
        let own = rbs_remote_settings::OwnCertificate {
            certificate: d.join("own-cert.pem"),
            key: d.join("own-key.pem"),
        };
        std::fs::write(&own.certificate, &g.cert_pem).unwrap();
        std::fs::write(&own.key, &g.key_pem).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&own.key, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let mut p = RemoteProfile {
            own_certificate: Some(own.clone()),
            ..RemoteProfile::new()
        };
        let prepared = prepare(&d, &mut p, NOW).unwrap();
        assert_eq!(prepared.source, Source::Own(own.certificate.clone()));
        let pin = TlsIdentity::from_pem(&g.cert_pem, &g.key_pem).unwrap();
        assert_eq!(prepared.identity, pin);
        assert!(
            !settings::key_file(&d).exists(),
            "nothing was copied or made"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn host_names_are_lower_case_with_dot_local() {
        for n in host_names() {
            assert!(n.chars().all(valid_host_char), "{n}");
            assert_eq!(n, n.to_ascii_lowercase());
        }
        let (names, addresses) = certificate_names();
        assert_eq!(names[0], "localhost");
        assert!(addresses.contains(&IpAddr::V4(Ipv4Addr::LOCALHOST)));
    }
}
