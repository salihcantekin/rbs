//! The control API's `tokens.*` methods on these settings (ADR-0044 decision 9, control
//! API spec "Initial method set"): the app answers them on its control thread for a
//! same-user IPC peer such as `rbs-cli token`, so a token made or revoked there works or
//! stops working in the running app at once. Same file, same lock and same rules as the
//! Settings window and `rbs-cli` without a running app.

use std::path::Path;

use rbs_api::request::{Empty, Response};
use rbs_api::request::{TokenCreated, TokenInfo, TokenList};
use rbs_api::{ApiError, Command, ErrorKind, TokenStore};

use crate::{Access, RemoteProfile, name_problem};

/// The answer to a `tokens.*` command.
#[derive(Debug)]
pub struct TokenAnswer {
    /// The reply to send.
    pub response: Response,
    /// The command changed the tokens.
    pub changed: bool,
}

/// Answers `command` if it is a `tokens.*` command, `None` otherwise. A change is read,
/// made and saved under the settings lock, which is never waited for: while another
/// program holds it the command fails and says to try again. After a change is saved,
/// `apply` gets the tokens as saved now, still under the lock, so a server that takes
/// them there (`rbs_api::TokenHandle::set`) gets changes in the order they were saved,
/// and before the reply is sent.
pub fn token_command(
    config: &Path,
    command: &Command,
    apply: impl FnOnce(TokenStore),
) -> Option<Result<TokenAnswer, ApiError>> {
    Some(match command {
        Command::TokensList => current(config).map(|p| TokenAnswer {
            response: Response::Tokens(TokenList {
                tokens: p
                    .tokens
                    .iter()
                    .map(|t| TokenInfo {
                        name: t.name.clone(),
                        scopes: t.scopes,
                    })
                    .collect(),
            }),
            changed: false,
        }),
        Command::TokensCreate(p) => change(config, apply, |profile| {
            let access = Access::of(p.scopes).ok_or_else(|| {
                ApiError::new(
                    ErrorKind::InvalidParams,
                    "scopes must be [\"read\"], [\"read\", \"control\"] or [\"read\", \"control\", \"admin\"]",
                )
            })?;
            let name = p.name.trim();
            if let Some(why) = name_problem(name) {
                return Err(ApiError::new(ErrorKind::InvalidParams, why));
            }
            if profile.index_of(name).is_some() {
                return Err(ApiError::new(
                    ErrorKind::Conflict,
                    format!("An access token named \"{name}\" exists. Choose another name."),
                ));
            }
            let token = profile
                .create_token(name, access)
                .map_err(|why| ApiError::new(ErrorKind::Failed, why))?;
            Ok(Response::TokenCreated(TokenCreated {
                name: name.to_owned(),
                scopes: access.scopes(),
                token: token.expose().to_owned(),
            }))
        }),
        Command::TokensRevoke(p) => change(config, apply, |profile| {
            let i = profile.index_of(&p.name).ok_or_else(|| {
                ApiError::new(
                    ErrorKind::NotFound,
                    format!("No access token is named \"{}\".", p.name.trim()),
                )
            })?;
            profile.revoke(i);
            Ok(Response::Done(Empty {}))
        }),
        _ => return None,
    })
}

/// Reads, changes and saves the settings under the lock.
fn change(
    config: &Path,
    apply: impl FnOnce(TokenStore),
    f: impl FnOnce(&mut RemoteProfile) -> Result<Response, ApiError>,
) -> Result<TokenAnswer, ApiError> {
    let lock = match crate::try_lock(config) {
        Ok(Some(lock)) => Some(lock),
        Ok(None) => {
            return Err(ApiError::new(
                ErrorKind::Failed,
                "The remote control settings are being changed right now (in RBS's Settings window or by rbs-cli). Try again.",
            ));
        }
        // A lock file that cannot be used does not stop the change (as in the app's
        // window): the lock only orders changes made at the same time.
        Err(_) => None,
    };
    let mut profile = current(config)?;
    let response = f(&mut profile)?;
    crate::save(config, &profile).map_err(|e| {
        ApiError::new(
            ErrorKind::Failed,
            format!(
                "The remote control settings could not be saved to {}: {e}",
                crate::file(config).display()
            ),
        )
    })?;
    apply(profile.store());
    drop(lock);
    Ok(TokenAnswer {
        response,
        changed: true,
    })
}

/// The settings as saved; a file that cannot be used is an error, never the defaults,
/// which would drop its tokens when saved.
fn current(config: &Path) -> Result<RemoteProfile, ApiError> {
    match crate::read(config) {
        Ok(p) => Ok(p.unwrap_or_default()),
        Err(why) => Err(ApiError::new(
            ErrorKind::Failed,
            format!("The remote control settings cannot be used: {why}. Fix or remove the file."),
        )),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // Tests fail by panicking.
mod tests {
    use std::path::PathBuf;

    use rbs_api::ScopeSet;
    use rbs_api::request::{CreateToken, TokenRef};
    use rbs_api::scope::Scope;

    use super::*;

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("rbs-remote-api-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    fn create(name: &str, scopes: ScopeSet) -> Command {
        Command::TokensCreate(CreateToken {
            name: name.into(),
            scopes,
        })
    }

    fn run(d: &Path, command: &Command) -> Option<Result<TokenAnswer, ApiError>> {
        token_command(d, command, |_| {})
    }

    fn kind(r: Option<Result<TokenAnswer, ApiError>>) -> ErrorKind {
        match r {
            Some(Err(e)) => e.kind,
            other => panic!("expected an error, got {other:?}"),
        }
    }

    /// Runs `command`, and returns the tokens it applied, checking they were applied
    /// while the lock was held.
    fn applied(d: &Path, command: &Command) -> (TokenAnswer, Option<TokenStore>) {
        let mut got = None;
        let a = token_command(d, command, |store| {
            assert!(
                crate::try_lock(d).unwrap().is_none(),
                "applied after unlocking"
            );
            got = Some(store);
        })
        .unwrap()
        .unwrap();
        assert_eq!(a.changed, got.is_some());
        (a, got)
    }

    #[test]
    fn create_list_and_revoke_change_the_file_and_apply_the_new_store_under_the_lock() {
        let d = dir("cycle");
        let saved = RemoteProfile {
            enabled: true,
            port: 4470,
            ..RemoteProfile::new()
        };
        crate::save(&d, &saved).unwrap();

        let (a, store) = applied(&d, &create(" Deck ", ScopeSet::PANEL));
        let Response::TokenCreated(created) = a.response else {
            panic!("not TokenCreated");
        };
        assert_eq!(created.name, "Deck");
        assert_eq!(created.scopes, ScopeSet::PANEL);
        let store = store.unwrap();
        assert_eq!(
            store.verify(&created.token).unwrap().scopes,
            ScopeSet::PANEL
        );
        let now = crate::read(&d).unwrap().unwrap();
        assert!(now.enabled, "other settings are kept");
        assert_eq!(now.port, 4470);
        assert_eq!(now.store().entries(), store.entries());

        let (a, store) = applied(&d, &Command::TokensList);
        assert!(store.is_none());
        let Response::Tokens(list) = a.response else {
            panic!("not Tokens");
        };
        assert_eq!(
            list.tokens,
            vec![TokenInfo {
                name: "Deck".into(),
                scopes: ScopeSet::PANEL
            }]
        );

        let (a, store) = applied(
            &d,
            &Command::TokensRevoke(TokenRef {
                name: "Deck".into(),
            }),
        );
        assert_eq!(a.response, Response::Done(Empty {}));
        assert!(store.unwrap().verify(&created.token).is_none());
        assert!(crate::read(&d).unwrap().unwrap().tokens.is_empty());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn bad_requests_get_typed_errors_and_change_nothing() {
        let d = dir("errors");
        run(&d, &create("Deck", ScopeSet::PANEL)).unwrap().unwrap();
        let before = std::fs::read_to_string(crate::file(&d)).unwrap();
        assert_eq!(
            kind(run(&d, &create("Deck", ScopeSet::PANEL))),
            ErrorKind::Conflict
        );
        assert_eq!(
            kind(run(&d, &create(" ", ScopeSet::PANEL))),
            ErrorKind::InvalidParams
        );
        assert_eq!(
            kind(run(&d, &create("a\nb", ScopeSet::PANEL))),
            ErrorKind::InvalidParams
        );
        // Only the access levels the app offers: never `tokens` or `plugins`.
        assert_eq!(
            kind(run(&d, &create("All", ScopeSet::ALL))),
            ErrorKind::InvalidParams
        );
        assert_eq!(
            kind(run(&d, &create("Ctl", ScopeSet::of([Scope::Control])))),
            ErrorKind::InvalidParams
        );
        assert_eq!(
            kind(run(
                &d,
                &Command::TokensRevoke(TokenRef {
                    name: "Nope".into()
                })
            )),
            ErrorKind::NotFound
        );
        assert_eq!(std::fs::read_to_string(crate::file(&d)).unwrap(), before);
        assert!(run(&d, &Command::StudioGet).is_none());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_change_while_another_program_holds_the_lock_fails_without_waiting() {
        let d = dir("busy");
        let held = crate::lock(&d).unwrap();
        assert_eq!(
            kind(run(&d, &create("Deck", ScopeSet::PANEL))),
            ErrorKind::Failed
        );
        // Reading needs no lock.
        assert!(run(&d, &Command::TokensList).unwrap().is_ok());
        drop(held);
        assert!(run(&d, &create("Deck", ScopeSet::PANEL)).unwrap().is_ok());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_file_that_cannot_be_used_is_left_alone() {
        let d = dir("bad");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(crate::file(&d), "enabled = yes").unwrap();
        assert_eq!(
            kind(run(&d, &create("Deck", ScopeSet::PANEL))),
            ErrorKind::Failed
        );
        assert_eq!(kind(run(&d, &Command::TokensList)), ErrorKind::Failed);
        assert_eq!(
            std::fs::read_to_string(crate::file(&d)).unwrap(),
            "enabled = yes"
        );
        let _ = std::fs::remove_dir_all(&d);
    }
}
