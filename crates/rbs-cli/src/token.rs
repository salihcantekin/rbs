//! `rbs-cli token list|create|revoke`: the access tokens apps use to control RBS
//! through the control API (ADR-0044 decision 9, control API spec "Authentication").
//!
//! They are kept in the same `remote-control.toml` as the desktop app's Settings,
//! Remote control, through the same code (`rbs-remote-settings`), so a token made here
//! shows there and the other way round. Only the token's hash is stored; the token
//! itself is printed once, alone on standard output, so a script can capture it.

use std::io::Write;
use std::path::Path;

use rbs_remote_settings::{self as settings, Access, RemoteProfile};

use crate::args::TokenCommand;

/// What a running app does with a change made here (until the app takes changes
/// over local IPC).
const APPLIES: &str =
    "A running RBS takes this change when its Settings window is opened next, or when it starts.";

/// Runs `command` on the settings in `config`. The token text goes to `out`, notes to
/// `notes`.
pub fn run(
    config: &Path,
    command: &TokenCommand,
    out: &mut dyn Write,
    notes: &mut dyn Write,
) -> Result<(), String> {
    // Held from the read to the save: the app may change the settings meanwhile.
    // Held from the read to the save only, not while printing (output may stall, and
    // the app does not wait for it).
    let lock = match command {
        TokenCommand::List => None,
        _ => Some(take_lock(config)?),
    };
    let mut profile = current(config)?;
    let written = |e: std::io::Error| e.to_string();
    match command {
        TokenCommand::List => {
            let state = if profile.enabled {
                format!("on, apps connect at {}", profile.address())
            } else {
                "off (turn it on in RBS: Settings, Remote control)".to_owned()
            };
            writeln!(out, "Remote control: {state}").map_err(written)?;
            if profile.tokens.is_empty() {
                writeln!(
                    out,
                    "No access tokens. Create one with: rbs-cli token create <name>"
                )
                .map_err(written)?;
            }
            for t in &profile.tokens {
                writeln!(out, "{}\t{}", t.name, Access::describe(t.scopes)).map_err(written)?;
            }
        }
        TokenCommand::Create { name, access } => {
            let token = profile.create_token(name, *access)?;
            save(config, &profile)?;
            drop(lock);
            let name = profile
                .tokens
                .last()
                .map(|t| t.name.clone())
                .unwrap_or_default();
            if let Err(e) = writeln!(out, "{}", token.expose()).and_then(|()| out.flush()) {
                // Nobody saw the token: a token nobody has must not stay valid.
                let _lock = take_lock(config)?;
                let mut now = current(config)?;
                if let Some(i) = now.index_of(&name) {
                    now.revoke(i);
                    save(config, &now)?;
                }
                return Err(format!(
                    "The token could not be printed ({e}), so \"{name}\" was not kept. Try again."
                ));
            }
            writeln!(
                notes,
                "Access token \"{name}\" ({}) created. It is shown only now: copy it into the app that will control RBS. {APPLIES}",
                access.label()
            )
            .map_err(written)?;
        }
        TokenCommand::Revoke { name } => {
            let i = profile.index_of(name).ok_or_else(|| {
                format!(
                    "No access token is named \"{}\". `rbs-cli token list` shows them.",
                    name.trim()
                )
            })?;
            let entry = profile.revoke(i);
            save(config, &profile)?;
            drop(lock);
            let name = entry.map(|e| e.name).unwrap_or_default();
            writeln!(
                notes,
                "Access token \"{name}\" revoked: apps using it stop working once RBS reads the change. {APPLIES}"
            )
            .map_err(written)?;
        }
    }
    Ok(())
}

fn take_lock(config: &Path) -> Result<settings::SettingsLock, String> {
    settings::lock(config).map_err(|e| {
        format!(
            "The remote control settings in {} could not be locked: {e}",
            config.display()
        )
    })
}

/// The settings as saved. A file that cannot be used is an error, not the defaults:
/// writing the defaults would drop the tokens and settings in it.
fn current(config: &Path) -> Result<RemoteProfile, String> {
    match settings::read(config) {
        Ok(p) => Ok(p.unwrap_or_default()),
        Err(why) => Err(format!(
            "The remote control settings cannot be used: {why}. Fix or remove the file. RBS sets such a file aside when it starts, not while it runs."
        )),
    }
}

fn save(config: &Path, profile: &RemoteProfile) -> Result<(), String> {
    settings::save(config, profile).map_err(|e| {
        format!(
            "The remote control settings could not be saved to {}: {e}",
            settings::file(config).display()
        )
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.
mod tests {
    use std::path::PathBuf;

    use rbs_api::scope::ScopeSet;

    use super::*;

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("rbs-cli-token-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    fn run_text(config: &Path, command: TokenCommand) -> Result<(String, String), String> {
        let (mut out, mut notes) = (Vec::new(), Vec::new());
        run(config, &command, &mut out, &mut notes)?;
        Ok((
            String::from_utf8(out).unwrap(),
            String::from_utf8(notes).unwrap(),
        ))
    }

    #[test]
    fn a_created_token_is_printed_once_and_verified_by_the_saved_store() {
        let d = dir("create");
        let (out, notes) = run_text(
            &d,
            TokenCommand::Create {
                name: "Stream Deck".into(),
                access: Access::Control,
            },
        )
        .unwrap();
        let token = out.trim_end();
        assert!(token.starts_with("rbs1_") && token.len() == 69, "{out}");
        assert!(notes.contains("\"Stream Deck\" (Control)"), "{notes}");
        let (saved, why) = settings::load(&d);
        assert_eq!(why, None);
        assert!(!saved.enabled);
        let store = saved.store();
        assert_eq!(store.verify(token).unwrap().scopes, ScopeSet::PANEL);
        let text = std::fs::read_to_string(settings::file(&d)).unwrap();
        assert!(!text.contains(token));

        let (list, _) = run_text(&d, TokenCommand::List).unwrap();
        assert!(list.contains("Remote control: off"), "{list}");
        assert!(list.contains("Stream Deck\tControl"), "{list}");
        assert!(!list.contains(token));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn revoke_removes_by_name_and_keeps_the_other_settings() {
        let d = dir("revoke");
        let mut p = RemoteProfile {
            enabled: true,
            port: 4470,
            ..RemoteProfile::new()
        };
        let deck = p.create_token("Deck", Access::Control).unwrap();
        let script = p.create_token("Script", Access::View).unwrap();
        settings::save(&d, &p).unwrap();

        let err = run_text(
            &d,
            TokenCommand::Revoke {
                name: "Nope".into(),
            },
        )
        .unwrap_err();
        assert!(err.contains("No access token is named \"Nope\""), "{err}");
        let (out, notes) = run_text(
            &d,
            TokenCommand::Revoke {
                name: " Deck ".into(),
            },
        )
        .unwrap();
        assert!(out.is_empty());
        assert!(notes.contains("\"Deck\" revoked"), "{notes}");

        let (saved, _) = settings::load(&d);
        assert!(saved.enabled);
        assert_eq!(saved.port, 4470);
        let store = saved.store();
        assert!(store.verify(deck.expose()).is_none());
        assert!(store.verify(script.expose()).is_some());
        let (list, _) = run_text(&d, TokenCommand::List).unwrap();
        assert!(
            list.contains("on, apps connect at ws://localhost:4470"),
            "{list}"
        );
        assert!(list.contains("Script\tView only"), "{list}");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_token_that_could_not_be_printed_is_not_kept() {
        struct Broken;
        impl Write for Broken {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                Err(std::io::ErrorKind::BrokenPipe.into())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let d = dir("broken");
        let command = TokenCommand::Create {
            name: "Deck".into(),
            access: Access::View,
        };
        let err = run(&d, &command, &mut Broken, &mut Vec::new()).unwrap_err();
        assert!(err.contains("was not kept"), "{err}");
        assert!(settings::load(&d).0.tokens.is_empty());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_taken_name_is_refused() {
        let d = dir("taken");
        let create = || TokenCommand::Create {
            name: "Deck".into(),
            access: Access::Full,
        };
        run_text(&d, create()).unwrap();
        let err = run_text(&d, create()).unwrap_err();
        assert!(err.contains("exists"), "{err}");
        assert_eq!(settings::load(&d).0.tokens.len(), 1);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_bad_file_is_not_replaced() {
        let d = dir("bad");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(settings::file(&d), "enabled = yes").unwrap();
        let err = run_text(
            &d,
            TokenCommand::Create {
                name: "Deck".into(),
                access: Access::View,
            },
        )
        .unwrap_err();
        assert!(err.contains("cannot be used"), "{err}");
        assert_eq!(
            std::fs::read_to_string(settings::file(&d)).unwrap(),
            "enabled = yes"
        );
        let _ = std::fs::remove_dir_all(&d);
    }
}
