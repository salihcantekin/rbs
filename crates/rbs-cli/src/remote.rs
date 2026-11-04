//! `rbs-cli remote fingerprint` (ADR-0046 decision 4): prints what clients on other
//! computers check before they send a token, for setups without the desktop app's
//! Settings window. It reads the stored certificate and key (or the user's own) and
//! opens no connection.

use std::io::Write;
use std::path::Path;

use rbs_api_ws::TlsIdentity;
use rbs_remote_settings as settings;

/// Prints the key pin and the certificate fingerprint for the settings in `config`.
pub fn fingerprint(config: &Path, out: &mut dyn Write) -> Result<(), String> {
    let profile = settings::read(config)?.unwrap_or_default();
    let (pem, source) = match &profile.own_certificate {
        Some(own) => (
            settings::read_own(own)?,
            format!("your own, {}", own.certificate.display()),
        ),
        None => match settings::read_generated(config)? {
            Some(pem) => (
                pem,
                format!("made by RBS, {}", settings::cert_file(config).display()),
            ),
            None => {
                return Err("RBS has not made a certificate yet. Turn on \"Allow access from other computers\" in Settings, Remote control.".into());
            }
        },
    };
    let identity = TlsIdentity::from_pem(&pem.certificate, &pem.key)
        .map_err(|why| format!("the certificate cannot be used: {why}"))?;
    writeln!(
        out,
        "Key pin:                 {}\nCertificate fingerprint: {}\nCertificate:             {source}\n\nClients should pin the key: it stays the same when the certificate is renewed.",
        identity.key_pin(),
        identity.fingerprint()
    )
    .map_err(|e| e.to_string())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.
mod tests {
    use super::*;

    fn dir(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("rbs-cli-remote-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    fn print(config: &Path) -> Result<String, String> {
        let mut out = Vec::new();
        fingerprint(config, &mut out)?;
        Ok(String::from_utf8(out).unwrap())
    }

    #[test]
    fn prints_the_stored_identity_and_says_when_there_is_none() {
        let d = dir("generated");
        assert!(print(&d).unwrap_err().contains("not made a certificate"));
        let g = rbs_api_ws::tls::generate(&["studio".into()], &[], 1_800_000_000).unwrap();
        settings::save_generated(
            &d,
            &settings::Pem {
                certificate: g.cert_pem.clone(),
                key: g.key_pem.clone(),
            },
        )
        .unwrap();
        let identity = TlsIdentity::from_pem(&g.cert_pem, &g.key_pem).unwrap();
        let text = print(&d).unwrap();
        assert!(text.contains(identity.key_pin()), "{text}");
        assert!(text.contains(identity.fingerprint()), "{text}");
        assert!(text.contains("made by RBS"), "{text}");
        assert!(!text.contains("PRIVATE KEY"));

        // A key that is not the certificate's.
        let other = rbs_api_ws::tls::generate(&["studio".into()], &[], 1_800_000_000).unwrap();
        settings::save_generated(
            &d,
            &settings::Pem {
                certificate: g.cert_pem,
                key: other.key_pem,
            },
        )
        .unwrap();
        assert!(print(&d).unwrap_err().contains("cannot be used"));
        let _ = std::fs::remove_dir_all(&d);
    }
}
