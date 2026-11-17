//! AI tools settings (ADR-0134): `ai-tools.toml` beside `remote-control.toml` with
//! "Start when RBS opens", the access and permission of the AI tools token, and its
//! entries (`kind = "ai_tools"`, the reserved name). The token itself is written to
//! `agent-token`, which `rbs-mcp` reads; RBS never shows it.
//!
//! `remote-control.toml` does not change, so an older RBS never sees the AI tools entry.
//! Both files are written under the same settings lock ([`crate::lock`]); the server's
//! token store is their union ([`union_store`]).

use std::path::{Path, PathBuf};

use rbs_api::{AI_TOOLS_NAME, Token, TokenEntry, TokenHash, TokenKind, TokenStore};
use serde::{Deserialize, Serialize};

use crate::{Access, RemoteProfile, writable_by_others, write_private};

/// The file format version.
const FORMAT: u32 = 1;

/// The file `rbs-mcp` reads the token from (ADR-0052 decision 3 default).
pub const AGENT_TOKEN_FILE: &str = "agent-token";

/// The AI tools settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AiToolsSettings {
    /// File format version (1).
    pub format_version: u32,
    /// "Start when RBS opens". Off by default.
    #[serde(default)]
    pub start_on_open: bool,
    /// What the AI tools token may do: `view`, `control` or `full` ([`Access`]).
    /// Control by default.
    #[serde(default = "control_word")]
    pub access: String,
    /// "Allow actions that cannot be undone". Off by default.
    #[serde(default)]
    pub irreversible: bool,
    /// The AI tools token's entries: at most one current, more only between the
    /// steps of a rotation (Start settles it).
    #[serde(default)]
    pub tokens: Vec<TokenEntry>,
}

fn control_word() -> String {
    Access::Control.word().to_owned()
}

impl Default for AiToolsSettings {
    fn default() -> Self {
        Self {
            format_version: FORMAT,
            start_on_open: false,
            access: control_word(),
            irreversible: false,
            tokens: Vec::new(),
        }
    }
}

impl AiToolsSettings {
    /// The access; Control when the word is unknown.
    pub fn access(&self) -> Access {
        Access::parse(&self.access).unwrap_or(Access::Control)
    }

    /// Sets the access and the permission, and changes the entries to match (a local
    /// store update: sessions of the token are closed when the server takes it, and
    /// `rbs-mcp` reconnects with what the token allows now). True when it changed.
    pub fn set_access(&mut self, access: Access, irreversible: bool) -> bool {
        let changed = self.access() != access || self.irreversible != irreversible;
        self.access = access.word().to_owned();
        self.irreversible = irreversible;
        for e in &mut self.tokens {
            e.scopes = access.scopes();
            e.irreversible = irreversible;
        }
        changed
    }

    fn problem(&self) -> Option<String> {
        if self.format_version != FORMAT {
            return Some(format!(
                "settings version {} is not supported",
                self.format_version
            ));
        }
        if Access::parse(&self.access).is_none() {
            return Some(format!("unknown access \"{}\"", self.access));
        }
        if self
            .tokens
            .iter()
            .any(|t| t.kind != TokenKind::AiTools || t.name != AI_TOOLS_NAME)
        {
            return Some("an entry is not the AI tools token".into());
        }
        None
    }
}

/// `ai-tools.toml` in `config`.
pub fn file(config: &Path) -> PathBuf {
    config.join("ai-tools.toml")
}

/// The token file in `config`.
pub fn token_file(config: &Path) -> PathBuf {
    config.join(AGENT_TOKEN_FILE)
}

/// The saved settings; the defaults when there are none. A file that cannot be used is
/// an error, never the defaults (which would drop its entries when saved).
pub fn read(config: &Path) -> Result<AiToolsSettings, String> {
    let path = file(config);
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(AiToolsSettings::default());
        }
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    if let Some(why) = writable_by_others(&path) {
        return Err(format!("{}: {why}", path.display()));
    }
    let s =
        toml::from_str::<AiToolsSettings>(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    match s.problem() {
        None => Ok(s),
        Some(why) => Err(format!("{}: {why}", path.display())),
    }
}

/// Writes the settings (user-only, replaced in one step), under the settings lock.
pub fn save(config: &Path, settings: &AiToolsSettings) -> std::io::Result<()> {
    std::fs::create_dir_all(config)?;
    let text = toml::to_string(settings).map_err(std::io::Error::other)?;
    write_private(&file(config), text.as_bytes())
}

/// The tokens the server accepts: the remote control tokens and the AI tools entries.
pub fn union_store(remote: &RemoteProfile, ai: &AiToolsSettings) -> TokenStore {
    let mut store = remote.store();
    for t in &ai.tokens {
        store.insert(t.clone());
    }
    store
}

/// What [`prepare_start`] did.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Prepared {
    /// A new token was made and written (no usable file, or it did not match).
    pub new_token: bool,
    /// The file held a token of another kind (a user token): replaced.
    pub other_kind: bool,
    /// A user token named `AI tools` was renamed to this.
    pub renamed: Option<String>,
}

/// Makes sure the AI tools token and its file exist and match, for Start (ADR-0134
/// decision 3). Call under the settings lock. Saves both files as needed:
///
/// - a user token named `AI tools` is renamed to the first free `AI tools (n)`;
/// - a token file that is missing, unreadable, or whose hash is not an AI tools entry
///   leads to a new token ([`rotate`]);
/// - every AI tools entry except the one matching the file is revoked, which settles a
///   crash in the middle of a rotation.
pub fn prepare_start(config: &Path) -> Result<Prepared, String> {
    let mut prepared = Prepared::default();
    let mut remote = crate::read(config)?.unwrap_or_default();
    if let Some(i) = remote.index_of(AI_TOOLS_NAME) {
        let mut n = 2;
        let free = loop {
            let name = format!("{AI_TOOLS_NAME} ({n})");
            if remote.index_of(&name).is_none() {
                break name;
            }
            n += 1;
        };
        remote.tokens[i].name.clone_from(&free);
        crate::save(config, &remote)
            .map_err(|e| format!("{}: {e}", crate::file(config).display()))?;
        prepared.renamed = Some(free);
    }
    let mut ai = read(config)?;
    let in_file = read_token(config);
    let current = in_file
        .as_ref()
        .map(Token::hash)
        .filter(|h| ai.tokens.iter().any(|e| e.hash == *h));
    match current {
        Some(hash) => {
            let before = ai.tokens.len();
            ai.tokens.retain(|e| e.hash == hash);
            if ai.tokens.len() != before {
                save(config, &ai).map_err(|e| format!("{}: {e}", file(config).display()))?;
            }
        }
        None => {
            prepared.other_kind = in_file
                .as_ref()
                .is_some_and(|t| remote.store().contains(&t.hash()));
            rotate(config)?;
            prepared.new_token = true;
        }
    }
    Ok(prepared)
}

/// "New token": makes a token, adds its hash and saves `ai-tools.toml`, writes the
/// token file, then revokes the old entries (ADR-0134 decision 3). A crash leaves at
/// worst an extra valid entry, never a file without one. If the file cannot be written
/// the new entry is revoked again. Call under the settings lock.
pub fn rotate(config: &Path) -> Result<(), String> {
    let saving = |e: std::io::Error| format!("{}: {e}", file(config).display());
    let mut ai = read(config)?;
    let old: Vec<TokenHash> = ai.tokens.iter().map(|e| e.hash).collect();
    let token = Token::generate()
        .map_err(|e| format!("The system could not make a random token ({e}). Try again."))?;
    ai.tokens.push(TokenEntry {
        kind: TokenKind::AiTools,
        irreversible: ai.irreversible,
        name: AI_TOOLS_NAME.to_owned(),
        scopes: ai.access().scopes(),
        hash: token.hash(),
    });
    save(config, &ai).map_err(saving)?;
    if let Err(e) = write_token(config, &token) {
        ai.tokens.retain(|t| t.hash != token.hash());
        let _ = save(config, &ai);
        return Err(format!(
            "The token file {} could not be written: {e}",
            token_file(config).display()
        ));
    }
    ai.tokens.retain(|t| !old.contains(&t.hash));
    save(config, &ai).map_err(saving)
}

/// The token in the token file, or `None` when it is missing or holds no token.
fn read_token(config: &Path) -> Option<Token> {
    let text = std::fs::read_to_string(token_file(config)).ok()?;
    Token::parse(text.trim())
}

/// Writes the token file: a new temporary file in the same directory, created
/// exclusively with its final permissions (Unix mode 0600 at creation; Windows
/// inheriting the user-only folder ACL), flushed and synced, renamed over the old one,
/// then the directory synced. On Windows a replace refused because `rbs-mcp` holds the
/// file open is tried a few more times.
fn write_token(config: &Path, token: &Token) -> std::io::Result<()> {
    let path = token_file(config);
    let mut last = None;
    for _ in 0..5 {
        match write_private(&path, token.expose().as_bytes()) {
            Ok(()) => {
                sync_dir(config);
                return Ok(());
            }
            Err(e) if cfg!(windows) && e.kind() == std::io::ErrorKind::PermissionDenied => {
                last = Some(e);
                std::thread::yield_now();
            }
            Err(e) => return Err(e),
        }
    }
    Err(last.unwrap_or_else(|| std::io::Error::other("the file could not be replaced")))
}

#[cfg(unix)]
fn sync_dir(dir: &Path) {
    if let Ok(d) = std::fs::File::open(dir) {
        let _ = d.sync_all();
    }
}

#[cfg(not(unix))]
fn sync_dir(_dir: &Path) {}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.
mod tests {
    use super::*;

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("rbs-ai-tools-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn never_started_means_no_file_and_no_entry() {
        let d = dir("never");
        let ai = read(&d).unwrap();
        assert!(ai.tokens.is_empty());
        assert!(!ai.start_on_open && !ai.irreversible);
        assert_eq!(ai.access(), Access::Control);
        assert!(!file(&d).exists() && !token_file(&d).exists());
    }

    #[test]
    fn start_makes_the_token_and_file_and_the_store_accepts_it() {
        let d = dir("start");
        let p = prepare_start(&d).unwrap();
        assert!(p.new_token);
        let ai = read(&d).unwrap();
        assert_eq!(ai.tokens.len(), 1);
        let token = read_token(&d).unwrap();
        let store = union_store(&RemoteProfile::new(), &ai);
        let e = store.verify(token.expose()).unwrap();
        assert_eq!(e.kind, TokenKind::AiTools);
        assert_eq!(e.scopes, Access::Control.scopes());
        assert!(!e.irreversible);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(token_file(&d))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        // remote-control.toml never gets the entry.
        assert!(crate::read(&d).unwrap().is_none());
        // A second Start keeps the matching token.
        let p = prepare_start(&d).unwrap();
        assert!(!p.new_token);
        assert_eq!(read_token(&d).unwrap(), token);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn start_settles_a_rotation_cut_short_and_rotates_a_mismatched_file() {
        let d = dir("settle");
        prepare_start(&d).unwrap();
        let first = read_token(&d).unwrap();
        // A crash after the new entry was saved, before the file was written.
        let mut ai = read(&d).unwrap();
        let mut extra = ai.tokens[0].clone();
        extra.hash = Token::from_bytes(&[9; 32]).hash();
        ai.tokens.push(extra);
        save(&d, &ai).unwrap();
        prepare_start(&d).unwrap();
        let ai = read(&d).unwrap();
        assert_eq!(ai.tokens.len(), 1);
        assert_eq!(ai.tokens[0].hash, first.hash());
        // A file whose token is not an AI tools entry: rotated.
        std::fs::write(token_file(&d), Token::from_bytes(&[8; 32]).expose()).unwrap();
        let p = prepare_start(&d).unwrap();
        assert!(p.new_token && !p.other_kind);
        let ai = read(&d).unwrap();
        assert_eq!(ai.tokens.len(), 1);
        assert_eq!(ai.tokens[0].hash, read_token(&d).unwrap().hash());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_user_token_in_the_file_is_another_kind_and_replaced() {
        let d = dir("kind");
        let mut remote = RemoteProfile::new();
        let user = remote.create_token("Deck", Access::Full).unwrap();
        crate::save(&d, &remote).unwrap();
        std::fs::write(token_file(&d), user.expose()).unwrap();
        let p = prepare_start(&d).unwrap();
        assert!(p.new_token && p.other_kind);
        assert_ne!(read_token(&d).unwrap(), user);
        // The user token itself stays.
        assert!(
            crate::read(&d)
                .unwrap()
                .unwrap()
                .store()
                .contains(&user.hash())
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_user_token_named_ai_tools_is_renamed() {
        let d = dir("rename");
        let mut remote = RemoteProfile::new();
        remote.tokens.push(TokenEntry {
            kind: TokenKind::User,
            irreversible: true,
            name: AI_TOOLS_NAME.into(),
            scopes: Access::Control.scopes(),
            hash: Token::from_bytes(&[1; 32]).hash(),
        });
        remote.create_token("AI tools (2)", Access::View).unwrap();
        crate::save(&d, &remote).unwrap();
        let p = prepare_start(&d).unwrap();
        assert_eq!(p.renamed.as_deref(), Some("AI tools (3)"));
        let saved = crate::read(&d).unwrap().unwrap();
        assert!(saved.index_of("AI tools (3)").is_some());
        assert!(saved.index_of(AI_TOOLS_NAME).is_none());
        let text = std::fs::read_to_string(crate::file(&d)).unwrap();
        assert!(
            !text.contains("kind"),
            "remote-control.toml never carries a kind"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn new_token_rotates_and_set_access_updates_the_entry() {
        let d = dir("rotate");
        prepare_start(&d).unwrap();
        let first = read_token(&d).unwrap();
        rotate(&d).unwrap();
        let second = read_token(&d).unwrap();
        assert_ne!(first, second);
        let ai = read(&d).unwrap();
        assert_eq!(ai.tokens.len(), 1);
        assert_eq!(ai.tokens[0].hash, second.hash());
        let mut ai = read(&d).unwrap();
        assert!(ai.set_access(Access::View, true));
        save(&d, &ai).unwrap();
        let ai = read(&d).unwrap();
        assert_eq!(ai.tokens[0].scopes, Access::View.scopes());
        assert!(ai.tokens[0].irreversible);
        assert!(!ai.clone().set_access(Access::View, true));
        let _ = std::fs::remove_dir_all(&d);
    }
}
