//! `rbs-cli token list|create|revoke`: the access tokens apps use to control RBS
//! through the control API (ADR-0044 decision 9, control API spec "Authentication").
//!
//! They are kept in the same `remote-control.toml` as the desktop app's Settings,
//! Remote control, through the same code (`rbs-remote-settings`), so a token made here
//! shows there and the other way round. Only the token's hash is stored; the token
//! itself is printed once, alone on standard output, so a script can capture it.
//!
//! While RBS runs with remote control on, `create` and `revoke` go to it over local
//! IPC (`tokens.create`, `tokens.revoke`): it changes the same file and the change is
//! in effect at once. Otherwise they change the file here, and a running RBS takes the
//! change later.

use std::io::Write;
use std::path::Path;

use rbs_api_ipc::Endpoint;
use rbs_remote_settings::{self as settings, Access, RemoteProfile};
use serde_json::json;

use crate::args::TokenCommand;
use crate::control::{Reach, RunningApp};

/// What a running app does with a change made in the file.
const APPLIES: &str =
    "RBS uses this change the next time remote control starts or its Settings window opens.";

/// Runs `command` on the settings in `config`, through the RBS serving `ipc` when one
/// does. The token text goes to `out`, notes to `notes`.
pub fn run(
    config: &Path,
    ipc: Option<&Endpoint>,
    command: &TokenCommand,
    out: &mut dyn Write,
    notes: &mut dyn Write,
) -> Result<(), String> {
    if !matches!(command, TokenCommand::List)
        && let Some(endpoint) = ipc
    {
        match RunningApp::connect(endpoint, "tokens", "token management") {
            Reach::Connected(app) => return live(app, endpoint, command, out, notes),
            Reach::NotRunning => {}
            // A revoke made only in the file would leave the token working in the
            // running RBS: refused, so a script does not take it as done.
            Reach::Unusable(why) if matches!(command, TokenCommand::Revoke { .. }) => {
                return Err(format!(
                    "RBS is running but could not be reached ({why}), so nothing was revoked. Try again."
                ));
            }
            Reach::Unusable(why) => {
                writeln!(
                    notes,
                    "The running RBS could not be reached ({why}), so the settings file is changed instead."
                )
                .map_err(|e| e.to_string())?;
            }
        }
    }
    in_file(config, command, out, notes)
}

/// After `tokens.create` got no answer: revokes the token by name if RBS made it, and
/// says what happened.
fn unanswered_create(endpoint: &Endpoint, name: &str, why: &str) -> String {
    let undone = match RunningApp::connect(endpoint, "tokens", "token management") {
        Reach::Connected(mut app) => match app.call("tokens.revoke", json!({ "name": name })) {
            Ok(_) => Ok(()),
            Err(e) if e.kind.as_deref() == Some("not_found") => Ok(()),
            Err(e) => Err(e.message),
        },
        Reach::NotRunning => Err("RBS is no longer running".to_owned()),
        Reach::Unusable(w) => Err(w),
    };
    match undone {
        Ok(()) => format!("{why}, so \"{name}\" was not kept. Try again."),
        Err(e) => format!(
            "{why}. RBS may have made \"{name}\" without showing it, and it could not be revoked ({e}). Check with: rbs-cli token list, and revoke it with: rbs-cli token revoke \"{name}\""
        ),
    }
}

/// Makes the change in the running RBS.
fn live(
    mut app: RunningApp,
    endpoint: &Endpoint,
    command: &TokenCommand,
    out: &mut dyn Write,
    notes: &mut dyn Write,
) -> Result<(), String> {
    let written = |e: std::io::Error| e.to_string();
    let refused = |e: crate::control::CallError| match e.kind.as_deref() {
        Some("not_found") => format!("{} `rbs-cli token list` shows them.", e.message),
        _ => e.message,
    };
    match command {
        TokenCommand::List => Ok(()),
        TokenCommand::Create { name, access } => {
            let r = match app.call(
                "tokens.create",
                json!({"name": name, "scopes": access.scopes()}),
            ) {
                Ok(r) => r,
                // No answer (timeout, connection lost): RBS may have made the token
                // all the same, and nobody saw it. Revoke it on a new connection.
                Err(e) if e.kind.is_none() => {
                    drop(app);
                    return Err(unanswered_create(endpoint, name.trim(), &e.message));
                }
                Err(e) => return Err(refused(e)),
            };
            let (Some(token), Some(name)) = (r["token"].as_str(), r["name"].as_str()) else {
                return Err("RBS answered without a token.".into());
            };
            if let Err(e) = writeln!(out, "{token}").and_then(|()| out.flush()) {
                // Nobody saw the token: a token nobody has must not stay valid.
                app.call("tokens.revoke", json!({ "name": name }))
                    .map_err(|r| {
                        format!(
                            "The token could not be printed ({e}) and \"{name}\" could not be revoked: {}. Revoke it with: rbs-cli token revoke \"{name}\"",
                            r.message
                        )
                    })?;
                return Err(format!(
                    "The token could not be printed ({e}), so \"{name}\" was not kept. Try again."
                ));
            }
            writeln!(
                notes,
                "Access token \"{name}\" ({}) created. It is shown only now: copy it into the app that will control RBS. RBS accepts it now.",
                access.label()
            )
            .map_err(written)
        }
        TokenCommand::Revoke { name } => {
            app.call("tokens.revoke", json!({ "name": name.trim() }))
                .map_err(refused)?;
            writeln!(
                notes,
                "Access token \"{}\" revoked: apps using it were disconnected and cannot connect with it again.",
                name.trim()
            )
            .map_err(written)
        }
    }
}

/// Makes the change in the settings file.
fn in_file(
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
        run(config, None, &command, &mut out, &mut notes)?;
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
        let err = run(&d, None, &command, &mut Broken, &mut Vec::new()).unwrap_err();
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

    /// A fresh IPC endpoint per test; Unix socket paths are short.
    fn endpoint(name: &str) -> Endpoint {
        let id = format!("{name}-{}", std::process::id());
        if cfg!(windows) {
            Endpoint(format!(r"\\.\pipe\rbs-cli-test-{id}").into())
        } else {
            let dir = std::env::temp_dir().join(format!("rbs-cli-ipc-{id}"));
            let _ = std::fs::remove_dir_all(&dir);
            Endpoint(dir.join("c.sock"))
        }
    }

    /// A running "RBS": the API thread, local IPC, and a control thread that answers
    /// `tokens.*` from the settings in `config` as the app does.
    struct App {
        ipc: rbs_api_ipc::IpcTransport,
        api: Option<rbs_api::ApiServer>,
        wake: std::sync::mpsc::Sender<bool>,
        control: Option<std::thread::JoinHandle<()>>,
    }

    impl App {
        fn start(config: &Path, endpoint: &Endpoint) -> App {
            use rbs_api::request::ClientInfo;
            use rbs_api::{
                ApiError, ApiOptions, ApiServer, Command, ControlTarget, ControlTransport,
                EventPublisher, ServerConfig, TokenHandle, TokenStore,
            };

            struct Tokens(PathBuf, TokenHandle);
            impl ControlTarget for Tokens {
                fn handle(
                    &mut self,
                    command: Command,
                    _: &mut EventPublisher,
                ) -> Result<rbs_api::Response, ApiError> {
                    match settings::token_command(&self.0, &command, |s| self.1.set(s)) {
                        Some(answer) => answer.map(|a| a.response),
                        None => Err(rbs_api::control::unsupported(&command)),
                    }
                }
            }

            let (wake, woken) = std::sync::mpsc::channel();
            let waker = std::sync::Mutex::new(wake.clone());
            let (api, control_end) = ApiServer::start(
                ServerConfig::new(ClientInfo::new("RBS", "test")).unwrap(),
                TokenStore::new(),
                ApiOptions::new(move || {
                    let _ = waker.lock().unwrap().send(true);
                }),
            )
            .unwrap();
            let mut target = Tokens(config.to_owned(), api.token_handle());
            let control = std::thread::spawn(move || {
                let mut events = EventPublisher::new();
                while let Ok(true) = woken.recv() {
                    while control_end.serve_one(&mut target, &mut events) {}
                }
            });
            let mut ipc =
                rbs_api_ipc::IpcTransport::new(rbs_api_ipc::IpcConfig::new(endpoint.clone()));
            ipc.start(api.inbox()).unwrap();
            App {
                ipc,
                api: Some(api),
                wake,
                control: Some(control),
            }
        }
    }

    impl Drop for App {
        fn drop(&mut self) {
            use rbs_api::ControlTransport;
            self.ipc.stop();
            self.api.take();
            let _ = self.wake.send(false);
            if let Some(c) = self.control.take() {
                let _ = c.join();
            }
            if !cfg!(windows) {
                let _ = std::fs::remove_dir_all(self.ipc.endpoint().0.parent().unwrap());
            }
        }
    }

    fn run_with(
        config: &Path,
        ipc: &Endpoint,
        command: TokenCommand,
    ) -> Result<(String, String), String> {
        let (mut out, mut notes) = (Vec::new(), Vec::new());
        run(config, Some(ipc), &command, &mut out, &mut notes)?;
        Ok((
            String::from_utf8(out).unwrap(),
            String::from_utf8(notes).unwrap(),
        ))
    }

    #[test]
    fn with_rbs_running_changes_go_to_it_and_take_effect_at_once() {
        let d = dir("live");
        let e = endpoint("live");
        let _app = App::start(&d, &e);
        let (out, notes) = run_with(
            &d,
            &e,
            TokenCommand::Create {
                name: "Deck".into(),
                access: Access::Full,
            },
        )
        .unwrap();
        let token = out.trim_end();
        assert!(token.starts_with("rbs1_"), "{out}");
        assert!(notes.contains("RBS accepts it now"), "{notes}");
        let saved = settings::read(&d).unwrap().unwrap();
        assert_eq!(
            saved.store().verify(token).unwrap().scopes,
            Access::Full.scopes()
        );

        let err = run_with(
            &d,
            &e,
            TokenCommand::Revoke {
                name: "Nope".into(),
            },
        )
        .unwrap_err();
        assert!(err.contains("`rbs-cli token list`"), "{err}");
        let (_, notes) = run_with(
            &d,
            &e,
            TokenCommand::Revoke {
                name: " Deck ".into(),
            },
        )
        .unwrap();
        assert!(notes.contains("were disconnected"), "{notes}");
        assert!(settings::read(&d).unwrap().unwrap().tokens.is_empty());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn without_rbs_running_the_file_is_changed() {
        let d = dir("norbs");
        let e = endpoint("norbs");
        let (_, notes) = run_with(
            &d,
            &e,
            TokenCommand::Create {
                name: "Deck".into(),
                access: Access::View,
            },
        )
        .unwrap();
        assert!(notes.contains("Settings window"), "{notes}");
        assert_eq!(settings::read(&d).unwrap().unwrap().tokens.len(), 1);
        let _ = std::fs::remove_dir_all(&d);
    }
}
