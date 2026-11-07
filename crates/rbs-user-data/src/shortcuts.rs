//! The user's shortcuts (`shortcuts.toml`, `kind = "shortcuts"`, format 1): their
//! changes to the default shortcuts. Spec: `docs/specs/shortcuts.md`; decision:
//! ADR-0060.
//!
//! The body, its limits and the keymap it gives belong to `rbs_ui::shortcut`
//! ([`ShortcutsDoc`]); this module makes it a document.

use rbs_ui::shortcut::ShortcutsDoc;

use crate::document::{self, Document};

/// Largest shortcuts file, in bytes (spec).
pub const MAX_SHORTCUTS_FILE: u64 = 64 * 1024;

impl Document for ShortcutsDoc {
    const KIND: &'static str = "shortcuts";
    const FORMAT: u32 = 1;
    const MAX_SIZE: u64 = MAX_SHORTCUTS_FILE;

    fn from_body(body: toml::Table) -> Result<Self, String> {
        let doc: ShortcutsDoc = document::from_body(body)?;
        doc.validate()?;
        Ok(doc)
    }

    fn to_body(&self) -> Result<toml::Table, String> {
        self.validate()?;
        document::body_of(self)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.
mod tests {
    use super::*;
    use crate::UserData;
    use rbs_ui::shortcut::{Chord, Scope};

    fn dir(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("rbs-shortcuts-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn missing_file_is_the_defaults_and_writes_round_trip() {
        let d = dir("round-trip");
        let data = UserData::new(&d);
        let (doc, err) = data.shortcuts();
        assert_eq!(doc, ShortcutsDoc::default());
        assert!(err.is_none());
        let mut doc = ShortcutsDoc::default();
        doc.add(
            "stream.toggle",
            Chord::parse("Ctrl+Alt+S").unwrap(),
            Scope::Global,
        );
        doc.remove("edit.undo", Chord::parse("Ctrl+Z").unwrap());
        let lock = data.try_lock().unwrap().unwrap();
        data.set_shortcuts(&lock, &doc).unwrap();
        let text = std::fs::read_to_string(d.join(crate::SHORTCUTS_FILE)).unwrap();
        assert!(text.contains("kind = \"shortcuts\""), "{text}");
        let (back, err) = data.shortcuts();
        assert!(err.is_none());
        assert_eq!(back, doc);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_bad_file_gives_the_defaults_and_is_kept() {
        let d = dir("bad");
        let path = d.join(crate::SHORTCUTS_FILE);
        let bad = "format = 1\nkind = \"shortcuts\"\n[[binding]]\ncommand = \"a.b\"\nkeys = \"Ctrl+Shift+1\"\n";
        std::fs::write(&path, bad).unwrap();
        let (doc, err) = UserData::new(&d).shortcuts();
        assert_eq!(doc, ShortcutsDoc::default());
        assert!(err.is_some());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), bad);
        let unknown = "format = 1\nkind = \"shortcuts\"\nextra = true\n";
        std::fs::write(&path, unknown).unwrap();
        assert!(UserData::new(&d).shortcuts().1.is_some());
        let big = format!(
            "format = 1\nkind = \"shortcuts\"\n# {}\n",
            "x".repeat(70_000)
        );
        std::fs::write(&path, big).unwrap();
        assert!(UserData::new(&d).shortcuts().1.is_some());
        let _ = std::fs::remove_dir_all(&d);
    }
}
