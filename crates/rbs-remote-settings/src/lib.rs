//! Remote control settings (glossary: Remote control, Access token): whether other apps
//! on this computer may control RBS through the control API (ADR-0044), the port they
//! connect to, and the access tokens they use. Kept in `remote-control.toml` in the
//! config directory ([`config_dir`]). Shared by the desktop app (Settings, Remote
//! control) and `rbs-cli token`, so both read and write the file the same way.
//! Toolkit-free and tested without a window.
//!
//! Off by default (ADR-0044 decision 11, ADR-0013): while off, nothing of the control
//! API is constructed. Only token hashes are stored (decision 9); the file is still
//! readable and writable by the current user only, because whoever can write it can add
//! a token of their own.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use rbs_api::scope::{Scope, ScopeSet};
use rbs_api::{Token, TokenEntry, TokenStore};
use serde::{Deserialize, Serialize};

/// The RBS configuration directory: `RBS_CONFIG_DIR` if set, else the platform's
/// per-user configuration directory plus `RBS` (or `rbs` on Linux).
pub fn config_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("RBS_CONFIG_DIR") {
        return PathBuf::from(d);
    }
    let home = || std::env::var_os("HOME").map(PathBuf::from);
    if cfg!(windows) {
        if let Some(d) = std::env::var_os("APPDATA") {
            return PathBuf::from(d).join("RBS");
        }
    } else if cfg!(target_os = "macos") {
        if let Some(h) = home() {
            return h.join("Library").join("Application Support").join("RBS");
        }
    } else if let Some(d) = std::env::var_os("XDG_CONFIG_HOME").filter(|d| !d.is_empty()) {
        return PathBuf::from(d).join("rbs");
    } else if let Some(h) = home() {
        return h.join(".config").join("rbs");
    }
    std::env::temp_dir().join("rbs")
}

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

    /// Its word on the command line (`rbs-cli token create --access <word>`).
    pub fn word(self) -> &'static str {
        match self {
            Access::View => "view",
            Access::Control => "control",
            Access::Full => "full",
        }
    }

    /// The access named by `word` ([`Access::word`]), ignoring case.
    pub fn parse(word: &str) -> Option<Access> {
        Access::ALL
            .into_iter()
            .find(|a| a.word().eq_ignore_ascii_case(word.trim()))
    }

    /// How a token's scopes are shown: the access label, or the scope names for a set
    /// no access level offers.
    pub fn describe(scopes: ScopeSet) -> String {
        Access::of(scopes).map_or_else(
            || {
                scopes
                    .iter()
                    .map(|s| s.name())
                    .collect::<Vec<_>>()
                    .join(", ")
            },
            |a| a.label().to_owned(),
        )
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

    /// Where the token named `name` is in the list.
    pub fn index_of(&self, name: &str) -> Option<usize> {
        self.tokens.iter().position(|t| t.name == name.trim())
    }

    /// Takes the token list from `saved` (the file as another program, such as
    /// `rbs-cli token`, left it) and keeps this profile's other settings. Returns
    /// whether the list changed.
    pub fn take_tokens(&mut self, saved: RemoteProfile) -> bool {
        if self.tokens == saved.tokens {
            return false;
        }
        self.tokens = saved.tokens;
        true
    }

    /// Removes the token at `index`; returns it, so the removal can be undone with
    /// [`RemoteProfile::restore`].
    pub fn revoke(&mut self, index: usize) -> Option<TokenEntry> {
        (index < self.tokens.len()).then(|| self.tokens.remove(index))
    }

    /// Puts a revoked token back at `index` (or last): undo of a revoke. Refused when a
    /// token of that name was made since.
    pub fn restore(&mut self, index: usize, entry: TokenEntry) -> Result<(), String> {
        if self.tokens.iter().any(|t| t.name == entry.name) {
            return Err(format!(
                "\"{}\" cannot be restored: another access token has that name.",
                entry.name
            ));
        }
        let index = index.min(self.tokens.len());
        self.tokens.insert(index, entry);
        Ok(())
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
    // Names are listed one per line (`rbs-cli token list`, the log).
    if name.chars().any(char::is_control) {
        return Some("Use a name without tabs, line breaks or other control characters.".into());
    }
    None
}

/// The settings file in `config`.
pub fn file(config: &Path) -> PathBuf {
    config.join("remote-control.toml")
}

/// The saved settings; the defaults (off) when there are none. A file that cannot be
/// read gives the defaults and the reason, so remote control stays off; a file with bad
/// content or that others can write is moved aside to `remote-control.toml.refused`.
pub fn load(config: &Path) -> (RemoteProfile, Option<String>) {
    let path = file(config);
    let why = match check(&path) {
        Ok(p) => return (p.unwrap_or_default(), None),
        Err(Refused::Unreadable(why)) => {
            return (
                RemoteProfile::new(),
                Some(format!("{}: {why}", path.display())),
            );
        }
        Err(Refused::Content(why)) => why,
    };
    // The next change writes a new file; the refused one is kept beside it.
    let kept = path.with_extension("toml.refused");
    let saved = match std::fs::rename(&path, &kept) {
        Ok(()) => format!(" It was moved to {}.", kept.display()),
        Err(_) => String::new(),
    };
    (
        RemoteProfile::new(),
        Some(format!("{}: {why}.{saved}", path.display())),
    )
}

/// The saved settings as they are now, without changing the file: `None` when there
/// is none, the reason when it cannot be used. For a program that already holds the
/// settings and wants changes another program made since.
pub fn read(config: &Path) -> Result<Option<RemoteProfile>, String> {
    let path = file(config);
    check(&path).map_err(|r| match r {
        Refused::Unreadable(why) | Refused::Content(why) => format!("{}: {why}", path.display()),
    })
}

enum Refused {
    /// The file could not be read (permissions, I/O): left as it is.
    Unreadable(String),
    /// Bad content, or others can write it.
    Content(String),
}

fn check(path: &Path) -> Result<Option<RemoteProfile>, Refused> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(Refused::Unreadable(e.to_string())),
    };
    if let Some(why) = writable_by_others(path) {
        return Err(Refused::Content(why));
    }
    let profile =
        toml::from_str::<RemoteProfile>(&text).map_err(|e| Refused::Content(e.to_string()))?;
    match profile.problem() {
        None => Ok(Some(profile)),
        Some(why) => Err(Refused::Content(why)),
    }
}

/// Why a settings file others may write is refused (anyone who can write it can add a
/// token of their own), or `None`.
#[cfg(unix)]
fn writable_by_others(path: &Path) -> Option<String> {
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(path).ok()?.permissions().mode();
    (mode & 0o022 != 0).then(|| {
        format!(
            "other users can change this file (mode {:o}), so its access tokens are not trusted",
            mode & 0o777
        )
    })
}

#[cfg(not(unix))]
fn writable_by_others(_path: &Path) -> Option<String> {
    None
}

/// Writes the settings, replacing the file in one step. On Unix the file is made
/// readable and writable by the current user only (mode 0600); on Windows the per-user
/// application data folder already is.
pub fn save(config: &Path, profile: &RemoteProfile) -> std::io::Result<()> {
    use std::io::Write;
    std::fs::create_dir_all(config)?;
    let text = toml::to_string(profile).map_err(std::io::Error::other)?;
    let path = file(config);
    // A name of its own, so two saves at once (the app and `rbs-cli`, which also
    // take [`lock`]) never write into each other's file.
    static SAVES: AtomicU64 = AtomicU64::new(0);
    let n = SAVES.fetch_add(1, Ordering::Relaxed);
    let tmp = path.with_extension(format!("toml.{}-{n}.tmp", std::process::id()));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut f = options.open(&tmp)?;
    let written = f.write_all(text.as_bytes()).and_then(|()| f.sync_all());
    drop(f);
    let done = written.and_then(|()| std::fs::rename(&tmp, &path));
    if done.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    done
}

/// Held while a program reads, changes and saves the settings, so changes the app and
/// `rbs-cli` make at the same time are applied one after the other instead of one
/// silently replacing the other. Released when dropped.
#[derive(Debug)]
pub struct SettingsLock(#[allow(dead_code)] std::fs::File);

/// Waits until no other program changes the settings in `config` and holds them
/// ([`SettingsLock`]): an advisory lock on `remote-control.lock` beside the file.
pub fn lock(config: &Path) -> std::io::Result<SettingsLock> {
    let f = lock_file(config)?;
    f.lock()?;
    Ok(SettingsLock(f))
}

/// [`lock`] without waiting: `None` while another program holds it. For the app's
/// window, which must not freeze.
pub fn try_lock(config: &Path) -> std::io::Result<Option<SettingsLock>> {
    let f = lock_file(config)?;
    match f.try_lock() {
        Ok(()) => Ok(Some(SettingsLock(f))),
        Err(std::fs::TryLockError::WouldBlock) => Ok(None),
        Err(std::fs::TryLockError::Error(e)) => Err(e),
    }
}

fn lock_file(config: &Path) -> std::io::Result<std::fs::File> {
    std::fs::create_dir_all(config)?;
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(config.join("remote-control.lock"))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.
mod tests {
    use super::*;

    fn dir(name: &str) -> PathBuf {
        let d =
            std::env::temp_dir().join(format!("rbs-remote-settings-{name}-{}", std::process::id()));
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
        let entry = p.revoke(0).unwrap();
        assert_eq!(entry.name, "Stream Deck");
        assert_eq!(p.revoke(0), None);
        assert!(p.store().verify(token.expose()).is_none());
        p.restore(0, entry.clone()).unwrap();
        assert!(p.store().verify(token.expose()).is_some());
        assert!(p.restore(0, entry).is_err());
    }

    #[test]
    fn access_words_parse_back() {
        for a in Access::ALL {
            assert_eq!(Access::parse(a.word()), Some(a));
        }
        assert_eq!(Access::parse(" FULL "), Some(Access::Full));
        assert_eq!(Access::parse("admin"), None);
        assert_eq!(Access::describe(ScopeSet::PANEL), "Control");
        assert_eq!(
            Access::describe(ScopeSet::of([Scope::Read, Scope::Tokens])),
            "read, tokens"
        );
    }

    #[test]
    fn tokens_are_taken_from_the_saved_file_and_other_settings_kept() {
        let mut shown = RemoteProfile {
            enabled: true,
            port: 4470,
            ..RemoteProfile::new()
        };
        shown.create_token("Deck", Access::Control).unwrap();
        let mut saved = RemoteProfile::new();
        saved.create_token("Script", Access::View).unwrap();
        assert!(shown.take_tokens(saved.clone()));
        assert!(!shown.take_tokens(saved));
        assert!(shown.enabled);
        assert_eq!(shown.port, 4470);
        assert_eq!(shown.index_of(" Script"), Some(0));
        assert_eq!(shown.index_of("Deck"), None);
    }

    #[test]
    fn empty_long_and_taken_names_are_refused() {
        let mut p = RemoteProfile::new();
        assert!(p.create_token(" ", Access::View).is_err());
        assert!(p.create_token(&"x".repeat(65), Access::View).is_err());
        p.create_token("Deck", Access::View).unwrap();
        assert!(p.create_token("Deck", Access::Full).is_err());
        assert!(p.create_token("Deck\tFull control", Access::View).is_err());
        assert!(p.create_token("Two\nlines", Access::View).is_err());
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

    #[cfg(unix)]
    #[test]
    fn a_file_others_can_write_is_refused() {
        use std::os::unix::fs::PermissionsExt;
        let d = dir("mode");
        let mut p = RemoteProfile {
            enabled: true,
            ..RemoteProfile::new()
        };
        p.create_token("Deck", Access::Control).unwrap();
        save(&d, &p).unwrap();
        std::fs::set_permissions(file(&d), std::fs::Permissions::from_mode(0o666)).unwrap();
        let (got, why) = load(&d);
        assert_eq!(got, RemoteProfile::new());
        assert!(why.unwrap().contains("other users"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn saves_under_the_lock_leave_no_temporary_files() {
        let d = dir("lock");
        let held = lock(&d).unwrap();
        assert!(
            try_lock(&d).unwrap().is_none(),
            "a second holder got the lock"
        );
        save(&d, &RemoteProfile::new()).unwrap();
        drop(held);
        assert!(try_lock(&d).unwrap().is_some());
        let _again = lock(&d).unwrap();
        let mut names: Vec<_> = std::fs::read_dir(&d)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        assert_eq!(names, ["remote-control.lock", "remote-control.toml"]);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn read_leaves_a_bad_file_where_it_is() {
        let d = dir("read");
        assert_eq!(read(&d), Ok(None));
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(file(&d), "enabled = yes").unwrap();
        assert!(read(&d).is_err());
        assert!(file(&d).exists());
        let p = RemoteProfile::new();
        save(&d, &p).unwrap();
        assert_eq!(read(&d), Ok(Some(p)));
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
        assert!(d.join("remote-control.toml.refused").exists());
        assert!(!file(&d).exists());
        let _ = std::fs::remove_dir_all(&d);
    }
}
