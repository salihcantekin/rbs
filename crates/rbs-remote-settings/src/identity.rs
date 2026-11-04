//! The TLS key and certificate for network access (ADR-0046 decision 4), as PEM text:
//! the generated ones kept next to `remote-control.toml`, or the user's own files.
//! Nothing here parses or makes them (that is `rbs_api_ws::tls`), so this crate needs
//! no crypto dependency.
//!
//! A key file that other users can read is refused on Unix, and the generated key file
//! is created readable by the current user only (on Windows the per-user application
//! data folder already is).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The generated key, in `config`.
pub fn key_file(config: &Path) -> PathBuf {
    config.join("remote-control-key.pem")
}

/// The generated certificate, in `config`.
pub fn cert_file(config: &Path) -> PathBuf {
    config.join("remote-control-cert.pem")
}

/// A certificate chain (the server's certificate first) and its key, as PEM.
#[derive(Clone, PartialEq, Eq)]
pub struct Pem {
    /// The certificate chain.
    pub certificate: String,
    /// The private key.
    pub key: String,
}

impl std::fmt::Debug for Pem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pem")
            .field("certificate", &self.certificate)
            .field("key", &"<redacted>")
            .finish()
    }
}

/// The user's own certificate chain and key (PEM files), read each time network
/// access starts and never copied.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OwnCertificate {
    /// The certificate chain file.
    pub certificate: PathBuf,
    /// The key file.
    pub key: PathBuf,
}

impl OwnCertificate {
    /// Why these paths cannot be used, or `None`.
    pub fn problem(&self) -> Option<String> {
        if self.certificate.as_os_str().is_empty() || self.key.as_os_str().is_empty() {
            return Some("Choose both the certificate file and the key file.".into());
        }
        if !self.certificate.is_absolute() || !self.key.is_absolute() {
            return Some("The certificate and key files must be given with full paths.".into());
        }
        None
    }
}

/// Saves a generated key and certificate in `config`: the key first, then the
/// certificate, each replacing its file in one step. Call it holding the settings lock
/// ([`crate::lock`]). A key without its certificate (a crash between the two) does not
/// match it, which is reported when they are used, like a missing file.
pub fn save_generated(config: &Path, pem: &Pem) -> std::io::Result<()> {
    std::fs::create_dir_all(config)?;
    crate::write_private(&key_file(config), pem.key.as_bytes())?;
    crate::write_private(&cert_file(config), pem.certificate.as_bytes())
}

/// The generated key and certificate in `config`: `None` when neither exists, the
/// reason in words when only one does or one cannot be used.
pub fn read_generated(config: &Path) -> Result<Option<Pem>, String> {
    let (cert, key) = (cert_file(config), key_file(config));
    match (cert.exists(), key.exists()) {
        (false, false) => Ok(None),
        (true, false) => Err(format!("{} is missing", key.display())),
        (false, true) => Err(format!("{} is missing", cert.display())),
        (true, true) => Ok(Some(Pem {
            certificate: read_text(&cert)?,
            key: read_key(&key)?,
        })),
    }
}

/// The user's own certificate chain and key.
pub fn read_own(own: &OwnCertificate) -> Result<Pem, String> {
    if let Some(why) = own.problem() {
        return Err(why);
    }
    Ok(Pem {
        certificate: read_text(&own.certificate)?,
        key: read_key(&own.key)?,
    })
}

fn read_text(path: &Path) -> Result<String, String> {
    std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))
}

fn read_key(path: &Path) -> Result<String, String> {
    if let Some(why) = readable_by_others(path) {
        return Err(format!("{}: {why}", path.display()));
    }
    read_text(path)
}

/// Why a key file others may read or change is refused, or `None`.
#[cfg(unix)]
fn readable_by_others(path: &Path) -> Option<String> {
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(path).ok()?.permissions().mode();
    (mode & 0o077 != 0).then(|| {
        format!(
            "other users can read or change this key file (mode {:o}); make it readable by you only (chmod 600)",
            mode & 0o777
        )
    })
}

#[cfg(not(unix))]
fn readable_by_others(_path: &Path) -> Option<String> {
    None
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.
mod tests {
    use super::*;

    fn dir(name: &str) -> PathBuf {
        let d =
            std::env::temp_dir().join(format!("rbs-remote-identity-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    fn pem() -> Pem {
        Pem {
            certificate: "-----BEGIN CERTIFICATE-----\nc\n-----END CERTIFICATE-----\n".into(),
            key: "-----BEGIN PRIVATE KEY-----\nk\n-----END PRIVATE KEY-----\n".into(),
        }
    }

    #[test]
    fn a_generated_identity_saves_and_reads_back_and_the_key_is_private() {
        let d = dir("roundtrip");
        assert_eq!(read_generated(&d), Ok(None));
        save_generated(&d, &pem()).unwrap();
        assert_eq!(read_generated(&d), Ok(Some(pem())));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(key_file(&d))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        // Replaced in place, no temporary files left.
        let other = Pem {
            key: "other".into(),
            ..pem()
        };
        save_generated(&d, &other).unwrap();
        assert_eq!(read_generated(&d).unwrap().unwrap().key, "other");
        let names: Vec<_> = std::fs::read_dir(&d)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert!(names.iter().all(|n| !n.ends_with(".tmp")), "{names:?}");
        assert!(
            !format!("{other:?}").contains("other\""),
            "the key is never printed"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn half_an_identity_is_reported() {
        let d = dir("half");
        save_generated(&d, &pem()).unwrap();
        std::fs::remove_file(cert_file(&d)).unwrap();
        assert!(read_generated(&d).unwrap_err().contains("missing"));
        save_generated(&d, &pem()).unwrap();
        std::fs::remove_file(key_file(&d)).unwrap();
        assert!(read_generated(&d).unwrap_err().contains("missing"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[cfg(unix)]
    #[test]
    fn a_key_others_can_read_is_refused() {
        use std::os::unix::fs::PermissionsExt;
        let d = dir("shared");
        save_generated(&d, &pem()).unwrap();
        std::fs::set_permissions(key_file(&d), std::fs::Permissions::from_mode(0o640)).unwrap();
        assert!(read_generated(&d).unwrap_err().contains("chmod 600"));
        let own = OwnCertificate {
            certificate: cert_file(&d),
            key: key_file(&d),
        };
        assert!(read_own(&own).unwrap_err().contains("chmod 600"));
        std::fs::set_permissions(key_file(&d), std::fs::Permissions::from_mode(0o400)).unwrap();
        assert_eq!(read_own(&own), Ok(pem()));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn own_certificate_paths_must_be_whole() {
        let own = OwnCertificate {
            certificate: "cert.pem".into(),
            key: "key.pem".into(),
        };
        assert!(own.problem().unwrap().contains("full paths"));
        assert!(read_own(&own).is_err());
        let empty = OwnCertificate {
            certificate: PathBuf::new(),
            key: std::env::temp_dir().join("k.pem"),
        };
        assert!(empty.problem().unwrap().contains("both"));
    }
}
