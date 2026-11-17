//! Where the token comes from (ADR-0052 decision 3 as amended by ADR-0134 decision 4):
//! an explicit `--token-file` wins over `RBS_TOKEN`, which wins over the default file,
//! `agent-token` in the RBS configuration directory. A file is read again on every
//! connection, so a rotation by "New token" needs no restart of the AI tool.

use std::io::Read;
use std::path::{Path, PathBuf};

use rbs_api::Token;

/// The default token file's name in the configuration directory.
pub const DEFAULT_FILE: &str = "agent-token";

/// Where the token is read from.
#[derive(Debug, Clone)]
pub enum TokenSource {
    /// A file, read on every connection.
    File(PathBuf),
    /// `RBS_TOKEN`, read once at start (its text, or `None` when it is no token).
    Env(Option<Token>),
}

impl TokenSource {
    /// `--token-file` if given, else `RBS_TOKEN` if set, else the default file in
    /// `config_dir`.
    pub fn choose(token_file: Option<PathBuf>, env: Option<String>, config_dir: &Path) -> Self {
        match (token_file, env) {
            (Some(path), _) => TokenSource::File(path),
            (None, Some(text)) => TokenSource::Env(Token::parse(text.trim())),
            (None, None) => TokenSource::File(config_dir.join(DEFAULT_FILE)),
        }
    }

    /// The token now. The error never contains the token or the file's text.
    pub fn read(&self) -> Result<Token, String> {
        match self {
            TokenSource::Env(Some(t)) => Ok(t.clone()),
            TokenSource::Env(None) => Err("RBS_TOKEN does not hold an RBS token".into()),
            TokenSource::File(path) => read_file(path),
        }
    }
}

/// Opens the file only to read it and closes it at once. On Windows, Rust opens files
/// sharing read, write and delete, so RBS can replace it while it is open (ADR-0134
/// decision 4).
fn read_file(path: &Path) -> Result<Token, String> {
    let mut file = std::fs::File::open(path).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => "the token file is missing".to_owned(),
        _ => format!("the token file cannot be read ({})", e.kind()),
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = file
            .metadata()
            .map_err(|e| format!("the token file cannot be read ({})", e.kind()))?
            .permissions()
            .mode();
        if mode & 0o077 != 0 {
            return Err("the token file can be read by other users, so it was not used".into());
        }
    }
    let mut text = String::new();
    // A token is 69 bytes; a larger file is not one.
    file.by_ref()
        .take(256)
        .read_to_string(&mut text)
        .map_err(|_| "the token file does not hold an RBS token".to_owned())?;
    drop(file);
    Token::parse(text.trim()).ok_or_else(|| "the token file does not hold an RBS token".into())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("rbs-mcp-token-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn write_private(path: &Path, text: &str) {
        std::fs::write(path, text).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
    }

    #[test]
    fn an_explicit_file_wins_over_the_environment() {
        let d = dir("explicit");
        let file = Token::from_bytes(&[1; 32]);
        let env = Token::from_bytes(&[2; 32]);
        let path = d.join("t");
        write_private(&path, file.expose());
        let source = TokenSource::choose(Some(path), Some(env.expose().into()), &d);
        assert_eq!(source.read().unwrap(), file);
        let source = TokenSource::choose(None, Some(env.expose().into()), &d);
        assert_eq!(source.read().unwrap(), env);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn the_file_is_read_again_each_time() {
        let d = dir("reread");
        let source = TokenSource::choose(None, None, &d);
        assert_eq!(source.read().unwrap_err(), "the token file is missing");
        let first = Token::from_bytes(&[3; 32]);
        write_private(&d.join(DEFAULT_FILE), first.expose());
        assert_eq!(source.read().unwrap(), first);
        let second = Token::from_bytes(&[4; 32]);
        write_private(&d.join(DEFAULT_FILE), &format!("{}\n", second.expose()));
        assert_eq!(source.read().unwrap(), second);
        write_private(&d.join(DEFAULT_FILE), "not a token");
        let e = source.read().unwrap_err();
        assert!(!e.contains("not a token"), "{e}");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[cfg(unix)]
    #[test]
    fn a_file_other_users_can_read_is_refused() {
        use std::os::unix::fs::PermissionsExt;
        let d = dir("mode");
        let path = d.join(DEFAULT_FILE);
        let t = Token::from_bytes(&[5; 32]);
        std::fs::write(&path, t.expose()).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let e = TokenSource::File(path).read().unwrap_err();
        assert!(e.contains("other users"), "{e}");
        assert!(!e.contains(t.expose()));
        let _ = std::fs::remove_dir_all(&d);
    }
}
