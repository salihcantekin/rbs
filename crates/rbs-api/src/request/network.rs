//! The network check of a call by its parameters (ADR-0052 decision 4).

use super::Command;
use crate::types::SourceKind;

impl Command {
    /// True when the call, by its parameters alone, could make RBS open a connection
    /// to a host (ADR-0052 decision 4, protocol 1.79): a browser source, or a location
    /// value that is not a local absolute path ([`crate::location::is_local_path`]).
    /// Checked on the API thread; a source's settings and target, whose meaning
    /// depends on the source's kind, are checked by the control target.
    pub fn reaches_network(&self) -> bool {
        use crate::location::is_local_path;
        match self {
            Command::SourceAdd(p) => {
                p.kind == SourceKind::Browser
                    || p.path.as_deref().is_some_and(|f| !is_local_path(f))
            }
            // Downloads from the catalog's hosts (ADR-0145 decision 9).
            Command::AddonInstall(_) | Command::AddonUpdate(_) => true,
            Command::RecordStartToFile(p) => !is_local_path(&p.path),
            Command::CollectionSetStingerFile(p) => !is_local_path(&p.path),
            Command::SettingsSet(p) => {
                let far = |f: &str| !f.is_empty() && !is_local_path(f);
                p.profiles
                    .iter()
                    .flatten()
                    .chain(p.vertical_profile.iter().flatten().map(|b| &**b))
                    .any(|r| far(&r.folder))
                    || p.replay
                        .as_ref()
                        .and_then(|r| r.folder.as_deref())
                        .is_some_and(far)
            }
            Command::Guest(g) => g.reaches_network(),
            _ => false,
        }
    }
}
