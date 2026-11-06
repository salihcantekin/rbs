//! Libraries of named entries, output profiles and the current selection (user data
//! spec, "Libraries").

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

mod common;

use common::TempDir;
use rbs_user_data::library::{DELETED_DIR, MAX_NAME};
use rbs_user_data::{Current, DataError, Entry, EntryId, OutputProfile, UserData};
use serde::{Deserialize, Serialize};

#[derive(Debug, PartialEq, Serialize, Deserialize)]
struct Recording {
    format_version: u32,
    folder: String,
    bitrate: u32,
}

fn rec(bitrate: u32) -> OutputProfile {
    let mut p = OutputProfile::new();
    p.set(
        "recording",
        &Recording {
            format_version: 1,
            folder: "/videos".to_owned(),
            bitrate,
        },
    )
    .unwrap();
    p
}

fn names(data: &UserData) -> Vec<String> {
    data.profiles()
        .list()
        .unwrap()
        .entries
        .into_iter()
        .map(|e| e.name)
        .collect()
}

#[test]
fn ids_are_made_from_names_and_validated() {
    assert_eq!(EntryId::from_name("Twitch 1080p!").as_str(), "twitch-1080p");
    assert_eq!(EntryId::from_name("  ").as_str(), "entry");
    assert_eq!(EntryId::from_name("Yayın Çekimi").as_str(), "yayin-cekimi");
    assert_eq!(EntryId::from_name("Größe 日本").as_str(), "gro-e");
    assert!(EntryId::from_name(&"x".repeat(200)).as_str().len() <= 56);
    for good in ["a", "twitch-1080p", "x2"] {
        assert!(EntryId::parse(good).is_some(), "{good}");
    }
    for bad in [
        "",
        "-a",
        "a-",
        "A",
        "a b",
        "a/b",
        "..",
        "a.toml",
        &"a".repeat(65),
    ] {
        assert!(EntryId::parse(bad).is_none(), "{bad}");
    }
}

#[test]
fn entries_are_created_listed_loaded_and_saved() {
    let d = TempDir::new("lib-basic");
    let data = UserData::new(d.path());
    let lib = data.profiles();
    assert!(lib.list().unwrap().entries.is_empty());

    let twitch = lib.create("  Twitch  ", &rec(6000)).unwrap();
    let youtube = lib.create("YouTube", &rec(9000)).unwrap();
    assert_eq!(twitch.as_str(), "twitch");
    assert_eq!(names(&data), ["Twitch", "YouTube"]);

    let mut e = lib.load(&twitch).unwrap();
    assert_eq!(e.name, "Twitch");
    assert_eq!(
        e.document
            .get::<Recording>("recording")
            .unwrap()
            .unwrap()
            .bitrate,
        6000
    );
    e.document = rec(4500);
    lib.save(&twitch, &e).unwrap();
    assert_eq!(lib.load(&twitch).unwrap().document, rec(4500));
    assert_eq!(lib.find("twitch").unwrap(), Some(twitch.clone()));
    assert_eq!(lib.find("youtube ").unwrap(), Some(youtube));
    assert_eq!(lib.find("Kick").unwrap(), None);

    assert!(matches!(
        lib.load(&EntryId::parse("kick").unwrap()),
        Err(DataError::NotFound(_))
    ));
    assert!(matches!(
        lib.save(
            &EntryId::parse("kick").unwrap(),
            &Entry {
                name: "Kick".to_owned(),
                document: rec(1)
            }
        ),
        Err(DataError::NotFound(_))
    ));
}

#[test]
fn names_are_unique_ignoring_case_and_checked() {
    let d = TempDir::new("lib-names");
    let lib = UserData::new(d.path()).profiles();
    let a = lib.create("Main", &rec(1)).unwrap();
    let b = lib.create("Other", &rec(2)).unwrap();
    assert!(matches!(
        lib.create("MAIN", &rec(3)),
        Err(DataError::NameTaken(_))
    ));
    assert!(matches!(
        lib.rename(&b, "main"),
        Err(DataError::NameTaken(_))
    ));
    // Its own name in another case is fine.
    lib.rename(&a, "MAIN").unwrap();
    for bad in ["", "   ", "a\nb", &"x".repeat(MAX_NAME + 1)] {
        assert!(
            matches!(lib.create(bad, &rec(1)), Err(DataError::BadName(_))),
            "{bad:?}"
        );
    }
    lib.create(&"é".repeat(MAX_NAME), &rec(1)).unwrap();
}

#[test]
fn rename_keeps_the_id_and_duplicate_gets_a_new_one() {
    let d = TempDir::new("lib-rename");
    let lib = UserData::new(d.path()).profiles();
    let id = lib.create("Stream", &rec(6000)).unwrap();
    lib.rename(&id, "Stream old").unwrap();
    assert_eq!(lib.load(&id).unwrap().name, "Stream old");
    // A new entry may take the old name; its id gets a number.
    let again = lib.create("Stream", &rec(1)).unwrap();
    assert_eq!(again.as_str(), "stream-2");
    let copy = lib.duplicate(&id, "Stream copy").unwrap();
    assert_eq!(lib.load(&copy).unwrap().document, rec(6000));
    assert_ne!(copy, id);
}

#[test]
fn delete_moves_the_file_aside() {
    let d = TempDir::new("lib-delete");
    let lib = UserData::new(d.path()).profiles();
    let id = lib.create("Gone", &rec(1)).unwrap();
    let first = lib.delete(&id).unwrap();
    assert_eq!(first, lib.dir().join(DELETED_DIR).join("gone.toml"));
    assert!(lib.list().unwrap().entries.is_empty());
    assert!(matches!(lib.delete(&id), Err(DataError::NotFound(_))));
    // Deleting a second entry with the same id keeps both copies.
    let id = lib.create("Gone", &rec(2)).unwrap();
    let second = lib.delete(&id).unwrap();
    assert_ne!(first, second);
    assert!(first.exists() && second.exists());
}

#[test]
fn unreadable_and_newer_files_are_reported_and_left_alone() {
    let d = TempDir::new("lib-problems");
    let lib = UserData::new(d.path()).profiles();
    lib.create("Good", &rec(1)).unwrap();
    let dir = lib.dir().to_owned();
    let newer = "format = 7\nkind = \"output-profile\"\nname = \"Future\"\n";
    std::fs::write(dir.join("future.toml"), newer).unwrap();
    std::fs::write(dir.join("broken.toml"), "format = ").unwrap();
    std::fs::write(dir.join("notes.txt"), "ignored").unwrap();
    std::fs::write(dir.join("Bad Name.toml"), "ignored: not an id").unwrap();
    let listing = lib.list().unwrap();
    assert_eq!(listing.entries.len(), 1);
    assert_eq!(listing.problems.len(), 2);
    assert!(
        listing
            .problems
            .iter()
            .any(|(_, e)| matches!(e, DataError::Newer { found: 7, .. }))
    );
    // The newer file's id stays taken and the file is untouched.
    let id = lib.create("Future", &rec(1)).unwrap();
    assert_eq!(id.as_str(), "future-2");
    assert_eq!(
        std::fs::read_to_string(dir.join("future.toml")).unwrap(),
        newer
    );
}

#[test]
fn export_and_import_round_trip_and_rename_on_clash() {
    let d = TempDir::new("lib-export");
    let data = UserData::new(d.path());
    let lib = data.profiles();
    let id = lib.create("Twitch", &rec(6000)).unwrap();
    let file = d.path().join("shared").join("twitch.rbs-profile.toml");
    lib.export(&id, &file).unwrap();

    let (copy, name) = lib.import(&file).unwrap();
    assert_eq!(name, "Twitch 2");
    assert_eq!(lib.load(&copy).unwrap().document, rec(6000));
    let (_, name) = lib.import(&file).unwrap();
    assert_eq!(name, "Twitch 3");

    // Into another user's data.
    let other = TempDir::new("lib-export-other");
    let (id2, name) = UserData::new(other.path())
        .profiles()
        .import(&file)
        .unwrap();
    assert_eq!((id2.as_str(), name.as_str()), ("twitch", "Twitch"));

    // A scene collection file is not a profile.
    let wrong = d.path().join("wrong.toml");
    std::fs::write(
        &wrong,
        "format = 1\nkind = \"scene-collection\"\nname = \"X\"\n",
    )
    .unwrap();
    assert!(matches!(
        lib.import(&wrong),
        Err(DataError::WrongKind { .. })
    ));
    // A file without a name is named after the file.
    let unnamed = d.path().join("Studio B.toml");
    std::fs::write(&unnamed, "format = 1\nkind = \"output-profile\"\n").unwrap();
    assert_eq!(lib.import(&unnamed).unwrap().1, "Studio B");
    // A long name that clashes still fits after numbering.
    let long = "y".repeat(MAX_NAME);
    lib.create(&long, &rec(1)).unwrap();
    let lid = lib.find(&long).unwrap().unwrap();
    let f = d.path().join("long.toml");
    lib.export(&lid, &f).unwrap();
    let (_, name) = lib.import(&f).unwrap();
    assert_eq!(name.chars().count(), MAX_NAME);
    assert!(name.ends_with(" 2"));
}

#[test]
fn profile_sections_are_typed_and_unknown_ones_are_kept() {
    let d = TempDir::new("lib-sections");
    let lib = UserData::new(d.path()).profiles();
    let dir = lib.dir().to_owned();
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("p.toml"),
        "format = 1\nkind = \"output-profile\"\nname = \"P\"\n\
         [recording]\nformat_version = 1\nfolder = \"/v\"\nbitrate = 5\n\
         [plugin-x]\nanything = [1, 2]\n",
    )
    .unwrap();
    let id = EntryId::parse("p").unwrap();
    let mut e = lib.load(&id).unwrap();
    assert_eq!(
        e.document.sections().collect::<Vec<_>>(),
        ["plugin-x", "recording"]
    );
    assert_eq!(
        e.document
            .get::<Recording>("recording")
            .unwrap()
            .unwrap()
            .bitrate,
        5
    );
    assert!(e.document.get::<Recording>("stream").unwrap().is_none());
    assert!(matches!(
        e.document.get::<Recording>("plugin-x"),
        Err(DataError::Invalid(_))
    ));
    e.document
        .set(
            "recording",
            &Recording {
                format_version: 1,
                folder: "/w".to_owned(),
                bitrate: 7,
            },
        )
        .unwrap();
    lib.save(&id, &e).unwrap();
    let again = lib.load(&id).unwrap();
    assert!(again.document.sections().any(|s| s == "plugin-x"));
    assert_eq!(
        again
            .document
            .get::<Recording>("recording")
            .unwrap()
            .unwrap()
            .bitrate,
        7
    );

    let mut p = OutputProfile::new();
    assert!(
        p.set(
            "name",
            &Recording {
                format_version: 1,
                folder: String::new(),
                bitrate: 1
            }
        )
        .is_err()
    );
    assert!(p.set("recording", &5u32).is_err());
    std::fs::write(
        dir.join("q.toml"),
        "format = 1\nkind = \"output-profile\"\nname = \"Q\"\nloose = 1\n",
    )
    .unwrap();
    assert!(matches!(
        lib.load(&EntryId::parse("q").unwrap()),
        Err(DataError::Invalid(_))
    ));
}

#[test]
fn the_current_selection_round_trips() {
    let d = TempDir::new("current");
    let data = UserData::new(d.path());
    let (c, problem) = data.current();
    assert_eq!(c, Current::default());
    assert!(problem.is_none());
    let want = Current::new(EntryId::parse("twitch"), EntryId::parse("talk-show"));
    data.set_current(&want).unwrap();
    assert_eq!(data.current().0, want);
    std::fs::write(
        d.path().join("current.toml"),
        "format = 1\nkind = \"current\"\nprofile = \"Not An Id\"\n",
    )
    .unwrap();
    let (c, problem) = data.current();
    assert_eq!(c, Current::default());
    assert!(matches!(problem, Some(DataError::Invalid(_))));
}
