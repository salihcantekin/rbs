//! Temporary browser profiles (ADR-0109 decision 10).
//!
//! A browser tree without a kept profile makes a private directory
//! `rbs-browser-<pid>-<nanos hex>-<attempt>` in the OS temporary directory and removes
//! it on exit. A tree that is killed, crashes, or whose CEF processes still hold files
//! when its removal gives up leaves the directory behind. The next tree to start removes
//! every such directory whose owner process is no longer alive
//! ([`stale_profiles`]). Naming and selection live here, without OS bindings, so they
//! are tested without CEF; the runner supplies the liveness check.

use std::path::{Path, PathBuf};

/// Prefix of a temporary profile directory's name.
pub const PREFIX: &str = "rbs-browser-";

/// The name of a temporary profile directory made by process `pid`.
pub fn profile_name(pid: u32, nanos: u32, attempt: u32) -> String {
    format!("{PREFIX}{pid}-{nanos:x}-{attempt}")
}

/// The owner process of a temporary profile directory named `name`, or `None` when the
/// name is not exactly one made by [`profile_name`].
pub fn owner_pid(name: &str) -> Option<u32> {
    let rest = name.strip_prefix(PREFIX)?;
    let mut parts = rest.split('-');
    let pid = parts.next()?;
    let nanos = parts.next()?;
    let attempt = parts.next()?;
    if parts.next().is_some()
        || !all_digits(pid, 10)
        || !all_digits(nanos, 16)
        || !all_digits(attempt, 10)
    {
        return None;
    }
    pid.parse().ok()
}

fn all_digits(s: &str, radix: u32) -> bool {
    !s.is_empty() && s.len() <= 10 && s.chars().all(|c| c.is_digit(radix))
}

/// Temporary profile directories in `dir` left by processes that are gone: the name is
/// one [`profile_name`] makes, the entry is a real directory (not a link), its owner is
/// not `own_pid`, and `alive` says the owner no longer runs. `alive` is asked once per
/// candidate; an owner whose state is unknown must count as alive. A directory that
/// cannot be read yields nothing.
pub fn stale_profiles(
    dir: &Path,
    own_pid: u32,
    mut alive: impl FnMut(u32) -> bool,
) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut stale = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid) = name.to_str().and_then(owner_pid) else {
            continue;
        };
        // `file_type` does not follow links: a planted link is never removed through.
        if pid == own_pid || !entry.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        if !alive(pid) {
            stale.push(entry.path());
        }
    }
    stale
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn a_made_name_gives_its_owner_back() {
        assert_eq!(owner_pid(&profile_name(4321, 0xdead_beef, 3)), Some(4321));
        assert_eq!(owner_pid(&profile_name(0, 0, 0)), Some(0));
    }

    #[test]
    fn other_names_have_no_owner() {
        for name in [
            "rbs-browser-",
            "rbs-browser-12",
            "rbs-browser-12-ab",
            "rbs-browser-12-ab-1-2",
            "rbs-browser-x2-ab-1",
            "rbs-browser-12-zz-1",
            "rbs-browser-12-ab-",
            "rbs-browser-20261120T152735Z-1.log",
            "rbs-browser-99999999999-ab-1",
            "rbs-remote-12-ab-1",
        ] {
            assert_eq!(owner_pid(name), None, "{name}");
        }
    }

    #[test]
    fn only_dead_owners_directories_are_stale() {
        let dir = std::env::temp_dir().join(format!("rbs-ipc-profile-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for (pid, attempt) in [(10, 0), (11, 0), (12, 1), (7, 0)] {
            std::fs::create_dir(dir.join(profile_name(pid, 0xab, attempt))).unwrap();
        }
        // A file with a profile's name and an unrelated directory are left alone.
        std::fs::write(dir.join(profile_name(13, 0xab, 0)), b"").unwrap();
        std::fs::create_dir(dir.join("rbs-browser-cache")).unwrap();
        // A link with a profile's name is never followed.
        #[cfg(unix)]
        std::os::unix::fs::symlink(
            dir.join("rbs-browser-cache"),
            dir.join(profile_name(14, 0xab, 0)),
        )
        .unwrap();
        let mut asked = Vec::new();
        let mut stale = stale_profiles(&dir, 7, |pid| {
            asked.push(pid);
            pid == 11
        });
        stale.sort();
        asked.sort_unstable();
        assert_eq!(asked, vec![10, 11, 12]);
        assert_eq!(
            stale,
            vec![
                dir.join(profile_name(10, 0xab, 0)),
                dir.join(profile_name(12, 0xab, 1)),
            ]
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_missing_directory_has_no_stale_profiles() {
        let dir = std::env::temp_dir().join(format!("rbs-ipc-no-dir-{}", std::process::id()));
        assert!(stale_profiles(&dir, 1, |_| false).is_empty());
    }
}
