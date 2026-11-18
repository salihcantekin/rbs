//! Conformance tests of the add-ons spec, with no network, no OS archive tools and no
//! CEF: an in-memory fetch, an in-memory archive and `std::fs` in a temporary
//! directory that fails on cue.

#![cfg(unix)] // Symbolic links in the test file system.
#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

mod support;

use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use rbs_add_ons::{AddOns, Entry, FetchError, InstallControl, InstallError, Installed, Status};
use rbs_core::time::TestClock;
use support::{Answer, Archive, MemFetch, MemUnpack, TestFs, URL, entry_for, entry_with};
use tempfile::TempDir;

/// A transfer failure case: its name, how the fetch answers, the expected error.
type Case = (
    &'static str,
    Box<dyn Fn(&MemFetch)>,
    fn(&InstallError) -> bool,
);

struct Rig {
    _tmp: TempDir,
    fs: Arc<TestFs>,
    addons: AddOns,
    clock: TestClock,
    cancel: Arc<AtomicBool>,
}

impl Rig {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let clock = TestClock::new();
        let fs = Arc::new(TestFs::new(clock.clone()));
        let addons = AddOns::new(tmp.path().join("add-ons"), Arc::clone(&fs) as _);
        Self {
            _tmp: tmp,
            fs,
            addons,
            clock,
            cancel: Arc::new(AtomicBool::new(false)),
        }
    }

    fn root(&self) -> std::path::PathBuf {
        self._tmp
            .path()
            .join("add-ons")
            .join("org.rbs.browser-engine")
    }

    fn install_with(
        &self,
        entry: &Entry,
        fetch: &MemFetch,
        seen: &mut Vec<Status>,
    ) -> Result<Installed, InstallError> {
        let mut progress = |s: Status| seen.push(s);
        self.addons.install(
            entry,
            fetch,
            &MemUnpack,
            InstallControl {
                cancel: &self.cancel,
                clock: &self.clock,
                progress: &mut progress,
            },
        )
    }

    fn install(&self, entry: &Entry, fetch: &MemFetch) -> Result<Installed, InstallError> {
        self.install_with(entry, fetch, &mut Vec::new())
    }

    /// Installs `archive` as `version` successfully.
    fn put(&self, version: &str) -> Entry {
        let bytes = Archive::runtime().bytes();
        let entry = entry_for(&bytes, version);
        self.install(&entry, &MemFetch::serving(URL, &bytes))
            .unwrap();
        entry
    }

    fn staging_is_empty(&self) -> bool {
        !self.root().join(".staging").exists()
    }
}

/// Installs an archive that must fail with `UnsafeArchive`.
fn refused(archive: Archive) -> String {
    let rig = Rig::new();
    let bytes = archive.bytes();
    let entry = entry_for(&bytes, "1.0.0");
    let result = rig.install(&entry, &MemFetch::serving(URL, &bytes));
    assert!(rig.addons.installed(&entry).is_none());
    assert!(rig.staging_is_empty());
    match result {
        Err(InstallError::UnsafeArchive(why)) => why,
        other => panic!("expected UnsafeArchive, got {other:?}"),
    }
}

#[test]
fn a_good_archive_installs_and_resolves() {
    let rig = Rig::new();
    let bytes = Archive::runtime().bytes();
    let entry = entry_for(&bytes, "156.6.0");
    assert_eq!(rig.addons.status(&entry), Status::NotInstalled);
    let mut seen = Vec::new();
    let done = rig
        .install_with(&entry, &MemFetch::serving(URL, &bytes), &mut seen)
        .unwrap();
    assert_eq!(done.version, "156.6.0");
    // icudtl.dat was only in Resources and is moved next to the library.
    assert!(done.library_dir.join("libcef.so").is_file());
    assert!(done.library_dir.join("icudtl.dat").is_file());
    assert!(done.resources_dir.join("resources.pak").is_file());
    assert_eq!(rig.addons.installed(&entry), Some(done));
    assert_eq!(
        rig.addons.status(&entry),
        Status::Installed {
            version: "156.6.0".into()
        }
    );
    assert!(seen.contains(&Status::Verifying) && seen.contains(&Status::Installing));
    assert!(rig.staging_is_empty());
}

#[test]
fn an_installed_version_is_not_fetched_again() {
    let rig = Rig::new();
    let entry = rig.put("1.0.0");
    let fetch = MemFetch::default();
    assert!(rig.install(&entry, &fetch).is_ok());
    assert_eq!(fetch.request_count(), 0);
}

#[test]
fn transfer_failures_never_install_and_never_touch_an_installed_version() {
    let good = Archive::runtime().bytes();
    let mut longer = good.clone();
    longer.push(0);
    let cases: Vec<Case> = vec![
        (
            "hash mismatch",
            Box::new(|f: &MemFetch| {
                let mut bad = Archive::runtime().bytes();
                let last = bad.len() - 1;
                bad[last] ^= 1;
                f.answer(URL, Answer::Body(bad));
            }),
            |e| *e == InstallError::HashMismatch,
        ),
        (
            "too large",
            Box::new(move |f: &MemFetch| f.answer(URL, Answer::Body(longer.clone()))),
            |e| *e == InstallError::TooLarge,
        ),
        (
            "short body",
            Box::new(|f: &MemFetch| f.answer(URL, Answer::Body(b"abc".to_vec()))),
            |e| matches!(e, InstallError::Network(_)),
        ),
        (
            "stall",
            Box::new(|f: &MemFetch| f.answer(URL, Answer::Stall(b"abc".to_vec()))),
            |e| *e == InstallError::Stalled,
        ),
        (
            "network error",
            Box::new(|f: &MemFetch| {
                f.answer(URL, Answer::Fail(FetchError::Network("reset".into())))
            }),
            |e| matches!(e, InstallError::Network(_)),
        ),
        (
            "redirect to another host",
            Box::new(|f: &MemFetch| {
                f.answer(URL, Answer::Redirect("https://evil.example.com/x".into()))
            }),
            |e| matches!(e, InstallError::HostNotAllowed(_)),
        ),
        (
            "redirect to http",
            Box::new(|f: &MemFetch| {
                f.answer(URL, Answer::Redirect("http://cdn.example.org/x".into()))
            }),
            |e| *e == InstallError::NotHttps,
        ),
        (
            "redirect loop",
            Box::new(|f: &MemFetch| f.answer(URL, Answer::Redirect(URL.into()))),
            |e| matches!(e, InstallError::Network(_)),
        ),
    ];
    for (name, setup, expect) in cases {
        let rig = Rig::new();
        let old = rig.put("1.0.0");
        let entry = entry_for(&good, "2.0.0");
        let fetch = MemFetch::default();
        setup(&fetch);
        let err = rig.install(&entry, &fetch).expect_err(name);
        assert!(expect(&err), "{name}: {err:?}");
        assert!(err.fail_reason().is_some(), "{name}");
        assert!(rig.addons.installed(&entry).is_none(), "{name}");
        assert!(
            rig.addons.installed(&old).is_some(),
            "{name}: old version touched"
        );
        assert!(rig.staging_is_empty(), "{name}");
    }
}

#[test]
fn an_allowed_redirect_is_followed() {
    let rig = Rig::new();
    let bytes = Archive::runtime().bytes();
    let entry = entry_for(&bytes, "1.0.0");
    let fetch = MemFetch::default();
    fetch.answer(
        URL,
        Answer::Redirect("https://assets.example.org/blob".into()),
    );
    fetch.answer("https://assets.example.org/blob", Answer::Body(bytes));
    assert!(rig.install(&entry, &fetch).is_ok());
    assert_eq!(fetch.request_count(), 2);
}

#[test]
fn cancel_mid_download_leaves_nothing_and_keeps_the_old_version() {
    let rig = Rig::new();
    let old = rig.put("1.0.0");
    let bytes = Archive::runtime().bytes();
    let entry = entry_for(&bytes, "2.0.0");
    let fetch = MemFetch::serving(URL, &bytes);
    *fetch.chunk.lock().unwrap() = Some(8);
    *fetch.after_first_read.lock().unwrap() = Some(Arc::clone(&rig.cancel));
    assert_eq!(rig.install(&entry, &fetch), Err(InstallError::Cancelled));
    assert_eq!(InstallError::Cancelled.fail_reason(), None);
    assert!(rig.addons.installed(&entry).is_none());
    assert!(rig.addons.installed(&old).is_some());
    assert!(rig.staging_is_empty());
}

#[test]
fn progress_is_throttled_on_the_injected_clock() {
    let rig = Rig::new();
    let bytes = Archive::runtime().bytes();
    let entry = entry_for(&bytes, "1.0.0");
    let fetch = MemFetch::serving(URL, &bytes);
    *fetch.chunk.lock().unwrap() = Some(1);
    *fetch.clock_per_read.lock().unwrap() = Some((rig.clock.clone(), Duration::from_millis(100)));
    let mut seen = Vec::new();
    rig.install_with(&entry, &fetch, &mut seen).unwrap();
    let updates = seen
        .iter()
        .filter(|s| matches!(s, Status::Downloading { .. }))
        .count();
    // One read per byte, 100 ms each: at most one update per 250 ms, plus the first
    // and the last.
    let reads = bytes.len() + 1;
    assert!(
        updates <= reads / 2 + 2,
        "{updates} updates for {reads} reads"
    );
    assert!(updates >= 3);
    assert_eq!(
        seen.iter()
            .rev()
            .find(|s| matches!(s, Status::Downloading { .. })),
        Some(&Status::Downloading {
            bytes: bytes.len() as u64,
            total: bytes.len() as u64
        })
    );
}

#[test]
fn not_enough_space_and_no_artifact_are_refused_before_downloading() {
    let rig = Rig::new();
    let bytes = Archive::runtime().bytes();
    let entry = entry_for(&bytes, "1.0.0");
    *rig.fs.free.lock().unwrap() = 10;
    let fetch = MemFetch::serving(URL, &bytes);
    assert_eq!(rig.install(&entry, &fetch), Err(InstallError::NoSpace));
    assert_eq!(fetch.request_count(), 0);

    let mut elsewhere = entry.clone();
    elsewhere.artifact = None;
    assert_eq!(
        rig.install(&elsewhere, &fetch),
        Err(InstallError::NotAvailable)
    );
    assert_eq!(InstallError::NotAvailable.fail_reason(), None);
}

#[test]
fn every_name_rule_refuses_the_archive() {
    let names: [&[u8]; 14] = [
        b"/abs",
        b"top/C:/x",
        b"top/a\\..\\..\\x",
        b"top/../x",
        b"top/a/./b",
        b"top/a.",
        b"top/a ",
        b"top/f.txt:stream",
        b"top/CON",
        b"top/nul.txt",
        b"top/a\x01",
        b"top/a*",
        b"top/\xff",
        b"top/.complete",
    ];
    for name in names {
        refused(Archive::runtime().raw_file(name));
    }
    // Case and normal-form clashes, exact duplicates.
    refused(
        Archive::runtime()
            .file("top/Resources/A.pak", b"1")
            .file("top/resources/a.pak", b"2"),
    );
    refused(
        Archive::runtime()
            .file("top/caf\u{e9}", b"1")
            .file("top/cafe\u{301}", b"2"),
    );
    refused(Archive::runtime().file("top/x", b"1").file("top/x", b"2"));
    // A file above the stripped directory.
    refused(Archive::runtime().file("loose", b"1"));
}

#[test]
fn link_rules() {
    // Escaping directly and through a chain.
    refused(Archive::runtime().symlink("top/l", "../../etc/passwd"));
    refused(Archive::runtime().symlink("top/l", "/etc/passwd"));
    refused(
        Archive::runtime()
            .dir("top/x/y/")
            .symlink("top/x/y/L", "../../Release")
            .symlink("top/x/y/A", "L/../../.."),
    );
    // Writing through an earlier link.
    refused(
        Archive::runtime()
            .symlink("top/l", "Release")
            .file("top/l/evil", b"x"),
    );
    // A link created after a deeper entry implied a directory there.
    refused(
        Archive::runtime()
            .file("top/d/f", b"x")
            .symlink("top/d", "Release"),
    );
    // Pointing at nothing.
    refused(Archive::runtime().symlink("top/l", "Release/missing"));
    // Too many links.
    let mut chain = Archive::runtime();
    for i in 0..17 {
        chain = chain.symlink(&format!("top/l{i}"), &format!("l{}", i + 1));
    }
    chain = chain.symlink("top/l17", "Release");
    refused(chain);
    // Hard links only to an earlier file.
    refused(Archive::runtime().hard_link("top/h", "top/Release/missing"));
    refused(Archive::runtime().hard_link("top/h", "top/Release"));
    // Devices and other special files.
    refused(Archive::runtime().other("top/dev"));
}

#[test]
fn a_framework_style_link_layout_installs() {
    let rig = Rig::new();
    let archive = Archive::runtime()
        .dir("top/Release/F.framework/Versions/A/")
        .file("top/Release/F.framework/Versions/A/F", b"bin")
        .symlink("top/Release/F.framework/Versions/Current", "A")
        .symlink("top/Release/F.framework/F", "Versions/Current/F")
        .hard_link("top/Release/copy.so", "top/Release/libcef.so");
    let bytes = archive.bytes();
    let entry = entry_for(&bytes, "1.0.0");
    let done = rig
        .install(&entry, &MemFetch::serving(URL, &bytes))
        .unwrap();
    let f = done.library_dir.join("F.framework/F");
    assert_eq!(std::fs::read(f).unwrap(), b"bin");
    assert_eq!(
        std::fs::read(done.library_dir.join("copy.so")).unwrap(),
        b"library"
    );
}

#[test]
fn caps_on_size_and_layout() {
    // More unpacked bytes than the catalog allows (4096).
    refused(Archive::runtime().file("top/big", &[0u8; 5000]));
    // library_dir missing, or a required file missing from both directories.
    let bytes = Archive::runtime().bytes();
    let rig = Rig::new();
    let entry = entry_with(
        &bytes,
        "1.0.0",
        &[("library_dir = \"Release\"", "library_dir = \"Nope\"")],
    );
    assert!(matches!(
        rig.install(&entry, &MemFetch::serving(URL, &bytes)),
        Err(InstallError::UnsafeArchive(_))
    ));
    let entry = entry_with(
        &bytes,
        "1.0.0",
        &[(
            "\"icudtl.dat\"]",
            "\"icudtl.dat\", \"v8_context_snapshot.bin\"]",
        )],
    );
    assert!(matches!(
        rig.install(&entry, &MemFetch::serving(URL, &bytes)),
        Err(InstallError::UnsafeArchive(_))
    ));
}

#[test]
fn setuid_setgid_and_sticky_bits_are_cleared() {
    let rig = Rig::new();
    let bytes = Archive::runtime()
        .file_mode("top/Release/tool", 0o7755)
        .bytes();
    let entry = entry_for(&bytes, "1.0.0");
    let done = rig
        .install(&entry, &MemFetch::serving(URL, &bytes))
        .unwrap();
    let mode = std::fs::metadata(done.library_dir.join("tool"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o7777, 0o755);
}

#[test]
fn a_held_lock_makes_install_clean_and_remove_busy() {
    let rig = Rig::new();
    let entry = rig.put("1.0.0");
    rig.fs
        .other_process
        .lock()
        .unwrap()
        .insert(rig.root().join(".lock"));
    let next = entry_for(&Archive::runtime().bytes(), "2.0.0");
    let fetch = MemFetch::serving(URL, &Archive::runtime().bytes());
    assert_eq!(rig.install(&next, &fetch), Err(InstallError::Busy));
    assert_eq!(fetch.request_count(), 0);
    assert_eq!(rig.addons.clean(&next), Err(InstallError::Busy));
    assert_eq!(rig.addons.remove(&entry), Err(InstallError::Busy));
    assert!(rig.addons.installed(&entry).is_some());
}

#[test]
fn an_unmarked_directory_of_the_same_version_is_replaced() {
    let rig = Rig::new();
    std::fs::create_dir_all(rig.root().join("1.0.0/junk")).unwrap();
    let bytes = Archive::runtime().bytes();
    let entry = entry_for(&bytes, "1.0.0");
    assert!(rig.addons.installed(&entry).is_none());
    rig.install(&entry, &MemFetch::serving(URL, &bytes))
        .unwrap();
    assert!(!rig.root().join("1.0.0/junk").exists());
}

#[test]
fn install_never_removes_versions_and_clean_keeps_current_previous_and_higher() {
    let rig = Rig::new();
    for v in ["1.0.0", "1.2.0", "1.10.0", "9.0.0"] {
        rig.put(v);
    }
    std::fs::create_dir_all(rig.root().join("not-a-version")).unwrap();
    let current = rig.put("2.0.0");
    for v in ["1.0.0", "1.2.0", "1.10.0", "9.0.0", "2.0.0"] {
        assert!(
            rig.root().join(v).join(".complete").exists(),
            "{v} kept by install"
        );
    }
    rig.addons.clean(&current).unwrap();
    let left: Vec<bool> = [
        "1.0.0",
        "1.2.0",
        "1.10.0",
        "2.0.0",
        "9.0.0",
        "not-a-version",
    ]
    .iter()
    .map(|v| rig.root().join(v).exists())
    .collect();
    // 1.10.0 is the previous version (numeric order), 9.0.0 is higher.
    assert_eq!(left, [false, false, true, true, true, true]);
}

#[test]
fn remove_deletes_the_marker_before_the_files() {
    let rig = Rig::new();
    let entry = rig.put("1.0.0");
    rig.fs.log.lock().unwrap().clear();
    rig.addons.remove(&entry).unwrap();
    let log = rig.fs.log.lock().unwrap().clone();
    let marker = log
        .iter()
        .position(|l| l.starts_with("remove_file") && l.ends_with("1.0.0/.complete"));
    let tree = log
        .iter()
        .position(|l| l.starts_with("remove_all") && l.ends_with("1.0.0"));
    assert!(marker.unwrap() < tree.unwrap(), "{log:?}");
    assert!(rig.addons.installed(&entry).is_none());
    assert_eq!(rig.addons.status(&entry), Status::NotInstalled);
}

#[test]
fn update_available_only_for_a_lower_version() {
    let rig = Rig::new();
    rig.put("1.0.0");
    let bytes = Archive::runtime().bytes();
    assert_eq!(
        rig.addons.status(&entry_for(&bytes, "2.0.0")),
        Status::UpdateAvailable {
            installed: "1.0.0".into(),
            available: "2.0.0".into()
        }
    );
    // Only a higher version on disk (a newer RBS build): not installed here.
    assert_eq!(
        rig.addons.status(&entry_for(&bytes, "0.9.0")),
        Status::NotInstalled
    );
    // A marker that names another version does not count.
    std::fs::create_dir_all(rig.root().join("3.0.0")).unwrap();
    std::fs::copy(
        rig.root().join("1.0.0/.complete"),
        rig.root().join("3.0.0/.complete"),
    )
    .unwrap();
    assert!(rig.addons.installed(&entry_for(&bytes, "3.0.0")).is_none());
}

#[test]
fn a_sharing_violation_is_retried_on_the_injected_clock() {
    let rig = Rig::new();
    let bytes = Archive::runtime().bytes();
    let entry = entry_for(&bytes, "1.0.0");
    rig.fs.sharing_violations.store(3, Ordering::Relaxed);
    rig.install(&entry, &MemFetch::serving(URL, &bytes))
        .unwrap();
    assert_eq!(rig.fs.pauses.load(Ordering::Relaxed), 3);

    let rig = Rig::new();
    rig.fs.sharing_violations.store(100, Ordering::Relaxed);
    assert!(matches!(
        rig.install(&entry, &MemFetch::serving(URL, &bytes)),
        Err(InstallError::Io(_))
    ));
    assert_eq!(rig.fs.pauses.load(Ordering::Relaxed), 9);
    assert!(rig.addons.installed(&entry).is_none());
}
