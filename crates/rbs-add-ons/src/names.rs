//! Entry names: one check, the same on every OS ([add-ons spec], "Entry names").
//!
//! Everything here works on the archive's own bytes. Nothing reads the disk, so the
//! rules for Windows (drive prefixes, reserved names, `:` streams, trailing dots) and
//! for case-insensitive file systems are tested on every platform.
//!
//! [add-ons spec]: https://github.com/salihcantekin/rbs/blob/main/docs/specs/add-ons.md

use std::collections::HashSet;
use std::fmt;
use std::path::{Path, PathBuf};

use unicode_normalization::UnicodeNormalization;

/// Longest single component, in bytes.
const MAX_COMPONENT: usize = 255;
/// Longest whole name, in bytes.
const MAX_NAME: usize = 1024;
/// Names Windows reserves for devices, compared without case and without extension.
const RESERVED: [&str; 22] = [
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// A checked relative path: one or more components, each passing the entry-name rules.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RelPath(Vec<String>);

impl RelPath {
    /// Checks a `/`-separated relative name (a catalog field such as `library_dir`,
    /// or an archive entry name). One trailing `/` is allowed (directories in tar).
    pub fn parse(name: &str) -> Result<Self, String> {
        if name.len() > MAX_NAME {
            return Err(format!("`{}`: longer than {MAX_NAME} bytes", lossy(name)));
        }
        if name.starts_with('/') {
            return Err(format!("`{}`: absolute path", lossy(name)));
        }
        let trimmed = name.strip_suffix('/').unwrap_or(name);
        if trimmed.is_empty() {
            return Err("empty name".to_owned());
        }
        let parts = trimmed
            .split('/')
            .map(|c| check_component(c).map(|()| c.to_owned()))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|why| format!("`{}`: {why}", lossy(name)))?;
        Ok(Self(parts))
    }

    /// Checks raw archive bytes: they must be UTF-8, then as [`RelPath::parse`].
    pub fn from_bytes(name: &[u8]) -> Result<Self, String> {
        let text = std::str::from_utf8(name)
            .map_err(|_| format!("`{}`: not UTF-8", String::from_utf8_lossy(name)))?;
        Self::parse(text)
    }

    /// Builds a path from components that already passed the checks.
    pub(crate) fn from_checked(parts: Vec<String>) -> Self {
        Self(parts)
    }

    /// One component, checked.
    pub fn single(name: &str) -> Result<Self, String> {
        check_component(name)?;
        Ok(Self(vec![name.to_owned()]))
    }

    /// The components.
    pub fn components(&self) -> &[String] {
        &self.0
    }

    /// Removes the first `n` components; `None` when nothing would be left.
    pub fn strip(&self, n: usize) -> Option<Self> {
        (self.0.len() > n).then(|| Self(self.0[n..].to_vec()))
    }

    /// `self` with `other` appended.
    pub fn join(&self, other: &RelPath) -> Self {
        let mut parts = self.0.clone();
        parts.extend(other.0.iter().cloned());
        Self(parts)
    }

    /// The path under `base`.
    pub fn under(&self, base: &Path) -> PathBuf {
        let mut path = base.to_path_buf();
        for c in &self.0 {
            path.push(c);
        }
        path
    }

    /// Every proper ancestor, shortest first (`a`, `a/b` for `a/b/c`).
    pub fn ancestors(&self) -> impl Iterator<Item = RelPath> + '_ {
        (1..self.0.len()).map(|n| Self(self.0[..n].to_vec()))
    }

    /// The key under which two names clash: NFC, then `str::to_lowercase`.
    pub(crate) fn fold(&self) -> String {
        self.to_string().nfc().collect::<String>().to_lowercase()
    }
}

impl fmt::Display for RelPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0.join("/"))
    }
}

/// Checks one path component against every rule of the spec.
pub(crate) fn check_component(c: &str) -> Result<(), String> {
    if c.is_empty() {
        return Err("empty component".to_owned());
    }
    if c == "." || c == ".." {
        return Err(format!("`{c}` component"));
    }
    if c.len() > MAX_COMPONENT {
        return Err(format!("a component is longer than {MAX_COMPONENT} bytes"));
    }
    if let Some(bad) = c
        .chars()
        .find(|ch| ch.is_control() || matches!(ch, '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|'))
    {
        return Err(format!("character {bad:?} is not allowed"));
    }
    if c.ends_with('.') || c.ends_with(' ') {
        return Err(format!("`{c}` ends with a dot or a space"));
    }
    let stem = c.split('.').next().unwrap_or(c);
    if RESERVED
        .iter()
        .any(|r| r.eq_ignore_ascii_case(stem.trim_end()))
    {
        return Err(format!("`{c}` is a reserved device name on Windows"));
    }
    Ok(())
}

/// Names seen so far in one archive, to refuse two that clash after folding.
#[derive(Debug, Default)]
pub(crate) struct NameSet(HashSet<String>);

impl NameSet {
    /// Records `name`; an error when it equals an earlier name after folding.
    pub(crate) fn insert(&mut self, name: &RelPath) -> Result<(), String> {
        if self.0.insert(name.fold()) {
            Ok(())
        } else {
            Err(format!("`{name}` clashes with an earlier name"))
        }
    }
}

fn lossy(name: &str) -> String {
    name.chars().take(80).collect()
}

#[cfg(test)]
mod tests {
    use super::{NameSet, RelPath};

    #[test]
    fn accepts_ordinary_names() {
        for name in [
            "Release/libcef.so",
            "Resources/locales/en-US.pak",
            "dir/",
            "Chromium Embedded Framework.framework/Versions/A/Resources",
            ".complete2",
            "CONSOLE.txt",
            "com10",
        ] {
            assert!(RelPath::parse(name).is_ok(), "{name}");
        }
    }

    #[test]
    fn refuses_every_listed_rule() {
        let long_component = "a".repeat(256);
        let long_name = format!("{}/x", "ab/".repeat(400));
        for name in [
            "/etc/passwd",
            "C:/x",
            "C:x",
            "\\\\?\\C:\\x",
            "a\\..\\b",
            "..\\x",
            "a/../b",
            "../x",
            "./x",
            "a//b",
            "a/./b",
            "",
            "/",
            "a/b.",
            "a/b ",
            "file.txt:stream",
            "a/CON",
            "a/con.txt",
            "nul",
            "Lpt9.log",
            "com1",
            "a\u{1}b",
            "a*",
            "a?",
            "a\"",
            "a<",
            "a>",
            "a|",
            long_component.as_str(),
            long_name.as_str(),
        ] {
            assert!(RelPath::parse(name).is_err(), "{name:?}");
        }
        assert!(RelPath::from_bytes(b"a/\xff").is_err());
    }

    #[test]
    fn names_that_differ_only_in_case_or_normal_form_clash() {
        let mut seen = NameSet::default();
        let parse = |n: &str| RelPath::parse(n).unwrap_or_else(|e| panic!("{e}"));
        assert!(seen.insert(&parse("Resources/A.pak")).is_ok());
        assert!(seen.insert(&parse("resources/a.pak")).is_err());
        assert!(seen.insert(&parse("Resources/A.pak")).is_err());
        // "é" precomposed and decomposed.
        assert!(seen.insert(&parse("caf\u{e9}")).is_ok());
        assert!(seen.insert(&parse("cafe\u{301}")).is_err());
        assert!(seen.insert(&parse("dir/")).is_ok());
        assert!(seen.insert(&parse("dir")).is_err());
    }

    #[test]
    fn strip_and_ancestors() {
        let p = RelPath::parse("top/Release/libcef.so").unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(
            p.strip(1).map(|p| p.to_string()).as_deref(),
            Some("Release/libcef.so")
        );
        assert!(p.strip(3).is_none());
        let a: Vec<String> = p.ancestors().map(|a| a.to_string()).collect();
        assert_eq!(a, ["top", "top/Release"]);
    }
}
