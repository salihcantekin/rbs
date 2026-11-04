//! Remote control settings (glossary: Remote control, Access token): whether other apps
//! on this computer may control RBS through the control API (ADR-0044), the port they
//! connect to, and the access tokens they use. Kept in `remote-control.toml` in the
//! config directory. Toolkit-free and tested without a window.
//!
//! Off by default (ADR-0044 decision 11, ADR-0013): while off, nothing of the control
//! API is constructed. Only token hashes are stored (decision 9); the file is still
//! readable and writable by the current user only, because whoever can write it can add
//! a token of their own.

use std::path::{Path, PathBuf};

use rbs_api::scope::{Scope, ScopeSet};
use rbs_api::{Token, TokenEntry, TokenStore};
use serde::{Deserialize, Serialize};

/// The file format version this build writes and reads.
const FORMAT: u32 = 1;

/// The port remote control listens on unless the user picks another. Not 4455, which
/// obs-websocket uses: both may run on the same computer.
pub const DEFAULT_PORT: u16 = 4456;

/// Lowest port the user may pick: the ones below need administrator rights on most
/// systems.
pub const MIN_PORT: u16 = 1024;

/// Longest token name, in characters.
pub const MAX_NAME: usize = 64;

/// What a token's app may do: a choice of scope sets the user understands. `plugins`
/// and `tokens` are never offered: token management stays in this window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    /// `read`: state, lists and events.
    View,
    /// `read` + `control`: scenes, sources, mute and record start and stop. What a
    /// hardware panel needs.
    Control,
    /// `read` + `control` + `admin`: also settings.
    Full,
}

impl Access {
    /// Every choice, in the order shown.
    pub const ALL: [Access; 3] = [Access::View, Access::Control, Access::Full];

    /// Its label.
    pub fn label(self) -> &'static str {
        match self {
            Access::View => "View only",
            Access::Control => "Control",
            Access::Full => "Full control",
        }
    }

    /// One line on what it allows.
    pub fn hint(self) -> &'static str {
        match self {
            Access::View => {
                "The app can see scenes, sources and recording state, and change nothing."
            }
            Access::Control => {
                "The app can change sources and start and stop recording. Right for Stream Deck and other panels. The default."
            }
            Access::Full => "The app can also change settings, such as the recording folder.",
        }
    }

    /// The scopes of a token with this access.
    pub fn scopes(self) -> ScopeSet {
        match self {
            Access::View => ScopeSet::of([Scope::Read]),
            Access::Control => ScopeSet::PANEL,
            Access::Full => ScopeSet::of([Scope::Read, Scope::Control, Scope::Admin]),
        }
    }

    /// The access of these scopes, or `None` for a set this window does not offer
    /// (written by another tool).
    pub fn of(scopes: ScopeSet) -> Option<Access> {
        Access::ALL.into_iter().find(|a| a.scopes() == scopes)
    }
}

/// Remote control settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteProfile {
    /// File format version (1).
    pub format_version: u32,
    /// Remote control is on. Off by default.
    pub enabled: bool,
    /// The loopback port apps connect to.
    pub port: u16,
    /// The tokens apps may use; only their hashes.
    #[serde(default)]
    pub tokens: Vec<TokenEntry>,
}

impl RemoteProfile {
    /// Off, on [`DEFAULT_PORT`], no tokens.
    pub fn new() -> Self {
        Self {
            format_version: FORMAT,
            enabled: false,
            port: DEFAULT_PORT,
            tokens: Vec::new(),
        }
    }

    /// Why these settings cannot be used, in words, or `None`.
    pub fn problem(&self) -> Option<String> {
        if self.format_version != FORMAT {
            return Some(format!(
                "settings version {} is not supported",
                self.format_version
            ));
        }
        if let Some(why) = port_problem(self.port) {
            return Some(why);
        }
        for (i, t) in self.tokens.iter().enumerate() {
            if let Some(why) = name_problem(&t.name) {
                return Some(why);
            }
            if self.tokens[..i].iter().any(|o| o.name == t.name) {
                return Some(format!("Two access tokens are named \"{}\".", t.name));
            }
        }
        None
    }

    /// Where apps connect: `ws://localhost:<port>`.
    pub fn address(&self) -> String {
        format!("ws://localhost:{}", self.port)
    }

    /// The tokens the server accepts.
    pub fn store(&self) -> TokenStore {
        let mut store = TokenStore::new();
        for t in &self.tokens {
            store.insert(t.clone());
        }
        store
    }

    /// Makes a token named `name` with `access`, keeps its hash, and returns the token
    /// itself: the only time it exists in readable form. Refuses an empty, too long or
    /// taken name with a sentence that says how to fix it.
    pub fn create_token(&mut self, name: &str, access: Access) -> Result<Token, String> {
        let name = name.trim();
        if let Some(why) = name_problem(name) {
            return Err(why);
        }
        if self.tokens.iter().any(|t| t.name == name) {
            return Err(format!(
                "An access token named \"{name}\" exists. Choose another name."
            ));
        }
        let token = Token::generate()
            .map_err(|e| format!("The system could not make a random token ({e}). Try again."))?;
        self.tokens.push(TokenEntry {
            name: name.to_owned(),
            scopes: access.scopes(),
            hash: token.hash(),
        });
        Ok(token)
    }

    /// Removes the token at `index`; returns its name.
    pub fn revoke(&mut self, index: usize) -> Option<String> {
        (index < self.tokens.len()).then(|| self.tokens.remove(index).name)
    }
}

impl Default for RemoteProfile {
    fn default() -> Self {
        Self::new()
    }
}

/// Why `port` cannot be used, or `None`.
pub fn port_problem(port: u16) -> Option<String> {
    (port < MIN_PORT).then(|| format!("Use a port from {MIN_PORT} to 65535."))
}

/// The port typed in `text`, or why it is not one.
pub fn parse_port(text: &str) -> Result<u16, String> {
    let port = text
        .trim()
        .parse::<u16>()
        .map_err(|_| format!("Use a port from {MIN_PORT} to 65535."))?;
    match port_problem(port) {
        Some(why) => Err(why),
        None => Ok(port),
    }
}

fn name_problem(name: &str) -> Option<String> {
    if name.trim().is_empty() {
        return Some("Give the access token a name, such as \"Stream Deck\".".into());
    }
    if name.chars().count() > MAX_NAME {
        return Some(format!("Use a name of at most {MAX_NAME} characters."));
    }
    None
}

fn file(config: &Path) -> PathBuf {
    config.join("remote-control.toml")
}

/// The saved settings; the defaults (off) when there are none. A file that cannot be
/// read gives the defaults and the reason, so remote control stays off.
pub fn load(config: &Path) -> (RemoteProfile, Option<String>) {
    let path = file(config);
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return (RemoteProfile::new(), None),
        Err(e) => {
            return (
                RemoteProfile::new(),
                Some(format!("{}: {e}", path.display())),
            );
        }
    };
    match toml::from_str::<RemoteProfile>(&text) {
        Ok(p) => match p.problem() {
            None => (p, None),
            Some(why) => (
                RemoteProfile::new(),
                Some(format!("{}: {why}", path.display())),
            ),
        },
        Err(e) => (
            RemoteProfile::new(),
            Some(format!("{}: {e}", path.display())),
        ),
    }
}

/// Writes the settings, replacing the file in one step. On Unix the file is made
/// readable and writable by the current user only (mode 0600); on Windows the per-user
/// application data folder already is.
pub fn save(config: &Path, profile: &RemoteProfile) -> std::io::Result<()> {
    use std::io::Write;
    std::fs::create_dir_all(config)?;
    let text = toml::to_string(profile).map_err(std::io::Error::other)?;
    let path = file(config);
    let tmp = path.with_extension("toml.tmp");
    let _ = std::fs::remove_file(&tmp);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut f = options.open(&tmp)?;
    f.write_all(text.as_bytes())?;
    f.sync_all()?;
    drop(f);
    std::fs::rename(&tmp, &path)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.
mod tests {
    use super::*;

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("rbs-app-remote-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn off_by_default_on_its_own_port() {
        let p = RemoteProfile::new();
        assert!(!p.enabled);
        assert_eq!(p.port, DEFAULT_PORT);
        assert_eq!(p.problem(), None);
        assert_eq!(p.address(), "ws://localhost:4456");
        assert!(p.store().entries().is_empty());
    }

    #[test]
    fn access_maps_to_scopes_and_back() {
        assert_eq!(Access::Control.scopes(), ScopeSet::PANEL);
        for a in Access::ALL {
            assert_eq!(Access::of(a.scopes()), Some(a));
            assert!(!a.scopes().contains(Scope::Tokens));
            assert!(!a.scopes().contains(Scope::Plugins));
        }
        assert_eq!(Access::of(ScopeSet::ALL), None);
    }

    #[test]
    fn a_created_token_is_verified_and_a_revoked_one_is_not() {
        let mut p = RemoteProfile::new();
        let token = p.create_token("  Stream Deck ", Access::Control).unwrap();
        assert_eq!(p.tokens[0].name, "Stream Deck");
        let store = p.store();
        let entry = store.verify(token.expose()).unwrap();
        assert_eq!(entry.scopes, ScopeSet::PANEL);
        assert!(store.verify("rbs1_0000").is_none());
        assert_eq!(p.revoke(0).as_deref(), Some("Stream Deck"));
        assert_eq!(p.revoke(0), None);
        assert!(p.store().verify(token.expose()).is_none());
    }

    #[test]
    fn empty_long_and_taken_names_are_refused() {
        let mut p = RemoteProfile::new();
        assert!(p.create_token(" ", Access::View).is_err());
        assert!(p.create_token(&"x".repeat(65), Access::View).is_err());
        p.create_token("Deck", Access::View).unwrap();
        assert!(p.create_token("Deck", Access::Full).is_err());
        assert_eq!(p.tokens.len(), 1);
    }

    #[test]
    fn ports_below_1024_and_text_are_refused() {
        assert_eq!(parse_port(" 4460 "), Ok(4460));
        assert!(parse_port("80").is_err());
        assert!(parse_port("70000").is_err());
        assert!(parse_port("port").is_err());
        let p = RemoteProfile {
            port: 22,
            ..RemoteProfile::new()
        };
        assert!(p.problem().is_some());
    }

    #[test]
    fn saved_settings_load_back_without_the_token_text() {
        let d = dir("save");
        assert_eq!(load(&d), (RemoteProfile::new(), None));
        let mut p = RemoteProfile {
            enabled: true,
            port: 4470,
            ..RemoteProfile::new()
        };
        let token = p.create_token("Deck", Access::Full).unwrap();
        save(&d, &p).unwrap();
        assert_eq!(load(&d), (p, None));
        let text = std::fs::read_to_string(file(&d)).unwrap();
        assert!(!text.contains(token.expose()));
        assert!(text.contains(&token.hash().to_hex()));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(file(&d)).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_bad_file_leaves_remote_control_off() {
        let d = dir("bad");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(file(&d), "format_version = 1\nenabled = true\nport = 80\n").unwrap();
        let (p, why) = load(&d);
        assert!(!p.enabled);
        assert!(why.is_some());
        std::fs::write(file(&d), "enabled = yes").unwrap();
        let (p, why) = load(&d);
        assert!(!p.enabled);
        assert!(why.is_some());
        let _ = std::fs::remove_dir_all(&d);
    }
}
