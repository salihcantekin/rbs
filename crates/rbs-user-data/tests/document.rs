//! Versioned documents (user data spec, "Documents").

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

mod common;

use common::TempDir;
use rbs_user_data::DataError;
use rbs_user_data::document::{self, Document, backup_path};

/// Format 3 of a test kind: `title` (was `heading` in 2, `head` in 1).
#[derive(Debug, PartialEq)]
struct Note {
    title: String,
}

impl Document for Note {
    const KIND: &'static str = "note";
    const FORMAT: u32 = 3;

    fn upgrade(from: u32, body: &mut toml::Table) -> Result<(), String> {
        let (old, new) = match from {
            1 => ("head", "heading"),
            2 => ("heading", "title"),
            _ => return Err(format!("format {from} cannot be read")),
        };
        let v = body.remove(old).ok_or("missing")?;
        body.insert(new.to_owned(), v);
        Ok(())
    }

    fn from_body(mut body: toml::Table) -> Result<Self, String> {
        match body.remove("title") {
            Some(toml::Value::String(title)) if body.is_empty() => Ok(Self { title }),
            _ => Err("bad note".to_owned()),
        }
    }

    fn to_body(&self) -> Result<toml::Table, String> {
        let mut t = toml::Table::new();
        t.insert("title".to_owned(), self.title.clone().into());
        Ok(t)
    }
}

fn note(t: &str) -> Note {
    Note {
        title: t.to_owned(),
    }
}

#[test]
fn a_document_round_trips_with_format_kind_and_name() {
    let text = document::render(&note("Hello"), Some("My note")).unwrap();
    assert!(text.contains("format = 3"), "{text}");
    assert!(text.contains("kind = \"note\""), "{text}");
    let read = document::parse::<Note>(&text).unwrap();
    assert_eq!(read.document, note("Hello"));
    assert_eq!(read.name.as_deref(), Some("My note"));
    assert_eq!(read.format, 3);
}

#[test]
fn older_formats_are_upgraded_step_by_step() {
    let read = document::parse::<Note>("format = 1\nkind = \"note\"\nhead = \"x\"\n").unwrap();
    assert_eq!(read.document, note("x"));
    assert_eq!(read.format, 1);
    let read = document::parse::<Note>("format = 2\nkind = \"note\"\nheading = \"y\"\n").unwrap();
    assert_eq!(read.document, note("y"));
}

#[test]
fn newer_formats_other_kinds_and_broken_files_are_refused() {
    assert!(matches!(
        document::parse::<Note>("format = 4\nkind = \"note\"\ntitle = \"x\"\n"),
        Err(DataError::Newer {
            found: 4,
            supported: 3
        })
    ));
    assert!(matches!(
        document::parse::<Note>("format = 3\nkind = \"theme\"\ntitle = \"x\"\n"),
        Err(DataError::WrongKind { .. })
    ));
    for bad in [
        "title = \"x\"\n",
        "format = 3\ntitle = \"x\"\n",
        "format = \"3\"\nkind = \"note\"\n",
        "format = 0\nkind = \"note\"\ntitle = \"x\"\n",
        "format = -1\nkind = \"note\"\ntitle = \"x\"\n",
    ] {
        assert!(
            matches!(document::parse::<Note>(bad), Err(DataError::NoVersion)),
            "{bad}"
        );
    }
    assert!(matches!(
        document::parse::<Note>("format = 3\nkind = \"note\"\ntitle = [\n"),
        Err(DataError::Syntax(_))
    ));
    assert!(matches!(
        document::parse::<Note>("format = 3\nkind = \"note\"\ntitle = 5\n"),
        Err(DataError::Invalid(_))
    ));
    assert!(matches!(
        document::parse::<Note>("format = 3\nkind = \"note\"\nname = 5\ntitle = \"x\"\n"),
        Err(DataError::Invalid(_))
    ));
}

#[test]
fn writing_never_replaces_a_newer_file_and_backs_up_an_older_one_once() {
    let d = TempDir::new("doc-write");
    let path = d.path().join("sub").join("n.toml");
    assert!(document::read::<Note>(&path).unwrap().is_none());

    // A new file, in a directory that did not exist.
    document::write(&path, &note("a"), None).unwrap();
    assert_eq!(
        document::read::<Note>(&path).unwrap().unwrap().document,
        note("a")
    );
    assert!(!path.with_extension("toml.tmp").exists());

    // A newer RBS wrote it: left alone.
    let newer = "format = 9\nkind = \"note\"\nwhatever = 1\n";
    std::fs::write(&path, newer).unwrap();
    assert!(matches!(
        document::write(&path, &note("b"), None),
        Err(DataError::Newer { found: 9, .. })
    ));
    assert_eq!(std::fs::read_to_string(&path).unwrap(), newer);

    // An older one: kept as <file>.format1.bak the first time only.
    let older = "format = 1\nkind = \"note\"\nhead = \"old\"\n";
    std::fs::write(&path, older).unwrap();
    document::write(&path, &note("c"), None).unwrap();
    let backup = backup_path(&path, 1);
    assert_eq!(std::fs::read_to_string(&backup).unwrap(), older);
    std::fs::write(&path, "format = 1\nkind = \"note\"\nhead = \"later\"\n").unwrap();
    document::write(&path, &note("d"), None).unwrap();
    assert_eq!(std::fs::read_to_string(&backup).unwrap(), older);
    assert_eq!(
        document::read::<Note>(&path).unwrap().unwrap().document,
        note("d")
    );
}

#[test]
fn reserved_keys_cannot_be_used_by_a_body() {
    struct Bad;
    impl Document for Bad {
        const KIND: &'static str = "bad";
        const FORMAT: u32 = 1;
        fn from_body(_: toml::Table) -> Result<Self, String> {
            Ok(Bad)
        }
        fn to_body(&self) -> Result<toml::Table, String> {
            let mut t = toml::Table::new();
            t.insert("kind".to_owned(), "x".into());
            Ok(t)
        }
    }
    assert!(matches!(
        document::render(&Bad, None),
        Err(DataError::Invalid(_))
    ));
}
