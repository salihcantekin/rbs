//! Capability model: what an adapter can do, and what the engine requires.
//!
//! Capabilities are plain data so they can be logged, shown in the UI, compared in
//! tests and sent across plugin boundaries. See `docs/specs/backend-ports.md`.

use std::borrow::Cow;
use std::fmt;

/// A namespaced capability name, for example `codec.video.encode` or `input.memory`.
///
/// Keys use lowercase dot-separated segments. Well-known keys are defined by each port
/// crate as constants.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CapabilityKey(Cow<'static, str>);

impl CapabilityKey {
    /// Creates a key from a static string, usable in `const` items.
    pub const fn new_static(key: &'static str) -> Self {
        Self(Cow::Borrowed(key))
    }

    /// Creates a key from an owned string (for example, read from a plugin manifest).
    pub fn new(key: impl Into<String>) -> Self {
        Self(Cow::Owned(key.into()))
    }

    /// The key as a string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for CapabilityKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The value of a capability.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CapabilityValue {
    /// A yes/no fact, for example `hardware = true`.
    Flag(bool),
    /// A single integer, for example a concurrent session limit.
    Int(i64),
    /// An inclusive integer range, for example supported widths `16..=8192`.
    IntRange {
        /// Smallest supported value.
        min: i64,
        /// Largest supported value.
        max: i64,
    },
    /// A set of supported symbolic values, for example codecs `["h264", "hevc", "av1"]`.
    Set(Vec<String>),
}

/// One declared capability.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Capability {
    /// What the capability describes.
    pub key: CapabilityKey,
    /// What the adapter supports for it.
    pub value: CapabilityValue,
}

/// Everything an adapter declared about itself after probing.
///
/// A key appears at most once; inserting an existing key replaces its value.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CapabilitySet {
    entries: Vec<Capability>,
}

impl CapabilitySet {
    /// An empty set: the adapter can do nothing on this machine.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds or replaces a capability and returns the set, for builder-style use.
    pub fn with(mut self, key: CapabilityKey, value: CapabilityValue) -> Self {
        self.insert(key, value);
        self
    }

    /// Adds or replaces a capability.
    pub fn insert(&mut self, key: CapabilityKey, value: CapabilityValue) {
        match self.entries.iter_mut().find(|c| c.key == key) {
            Some(existing) => existing.value = value,
            None => self.entries.push(Capability { key, value }),
        }
    }

    /// Looks up a capability value by key.
    pub fn get(&self, key: &CapabilityKey) -> Option<&CapabilityValue> {
        self.entries
            .iter()
            .find(|c| &c.key == key)
            .map(|c| &c.value)
    }

    /// True when the adapter declared nothing (typically: hardware or library missing).
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Iterates over all declared capabilities in insertion order.
    pub fn iter(&self) -> impl Iterator<Item = &Capability> {
        self.entries.iter()
    }
}

/// A condition on one capability.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Constraint {
    /// The key must be declared, with any value.
    Present,
    /// A flag must have this value.
    Flag(bool),
    /// An integer, or the top of a range, must be at least this value.
    AtLeast(i64),
    /// An integer, or the bottom of a range, must be at most this value.
    AtMost(i64),
    /// A set must contain this symbol.
    Includes(String),
}

impl Constraint {
    fn is_met_by(&self, value: &CapabilityValue) -> bool {
        match (self, value) {
            (Constraint::Present, _) => true,
            (Constraint::Flag(want), CapabilityValue::Flag(have)) => want == have,
            (Constraint::AtLeast(n), CapabilityValue::Int(v)) => v >= n,
            (Constraint::AtLeast(n), CapabilityValue::IntRange { max, .. }) => max >= n,
            (Constraint::AtMost(n), CapabilityValue::Int(v)) => v <= n,
            (Constraint::AtMost(n), CapabilityValue::IntRange { min, .. }) => min <= n,
            (Constraint::Includes(symbol), CapabilityValue::Set(items)) => {
                items.iter().any(|item| item == symbol)
            }
            _ => false,
        }
    }
}

impl fmt::Display for Constraint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Constraint::Present => f.write_str("is present"),
            Constraint::Flag(v) => write!(f, "is {v}"),
            Constraint::AtLeast(n) => write!(f, ">= {n}"),
            Constraint::AtMost(n) => write!(f, "<= {n}"),
            Constraint::Includes(s) => write!(f, "includes {s:?}"),
        }
    }
}

/// A constraint that a capability set failed to meet, kept for diagnostics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unmet {
    /// The key that was checked.
    pub key: CapabilityKey,
    /// The constraint that failed.
    pub constraint: Constraint,
    /// The declared value, or `None` if the key was missing.
    pub found: Option<CapabilityValue>,
}

impl fmt::Display for Unmet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.found {
            Some(value) => write!(f, "{} {} (found {value:?})", self.key, self.constraint),
            None => write!(f, "{} {} (not declared)", self.key, self.constraint),
        }
    }
}

/// What the engine needs from an adapter for a task. All constraints must hold.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Requirement {
    constraints: Vec<(CapabilityKey, Constraint)>,
}

impl Requirement {
    /// A requirement with no constraints; any adapter satisfies it.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a constraint and returns the requirement, for builder-style use.
    pub fn with(mut self, key: CapabilityKey, constraint: Constraint) -> Self {
        self.constraints.push((key, constraint));
        self
    }

    /// The constraints in the order they were added.
    pub fn constraints(&self) -> &[(CapabilityKey, Constraint)] {
        &self.constraints
    }

    /// True when `caps` meets every constraint.
    pub fn is_satisfied_by(&self, caps: &CapabilitySet) -> bool {
        self.constraints
            .iter()
            .all(|(key, c)| caps.get(key).is_some_and(|v| c.is_met_by(v)))
    }

    /// The constraints `caps` fails to meet, for logs and diagnostics.
    pub fn unmet(&self, caps: &CapabilitySet) -> Vec<Unmet> {
        self.constraints
            .iter()
            .filter_map(|(key, constraint)| {
                let found = caps.get(key);
                let ok = found.is_some_and(|v| constraint.is_met_by(v));
                (!ok).then(|| Unmet {
                    key: key.clone(),
                    constraint: constraint.clone(),
                    found: found.cloned(),
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CODECS: CapabilityKey = CapabilityKey::new_static("codec.video.encode");
    const WIDTH: CapabilityKey = CapabilityKey::new_static("video.width");
    const HARDWARE: CapabilityKey = CapabilityKey::new_static("adapter.hardware");
    const SESSIONS: CapabilityKey = CapabilityKey::new_static("codec.sessions.max");

    fn encoder_caps() -> CapabilitySet {
        CapabilitySet::new()
            .with(
                CODECS,
                CapabilityValue::Set(vec!["h264".into(), "hevc".into()]),
            )
            .with(WIDTH, CapabilityValue::IntRange { min: 16, max: 4096 })
            .with(HARDWARE, CapabilityValue::Flag(true))
            .with(SESSIONS, CapabilityValue::Int(3))
    }

    #[test]
    fn insert_replaces_existing_key() {
        let mut caps = CapabilitySet::new();
        caps.insert(HARDWARE, CapabilityValue::Flag(false));
        caps.insert(HARDWARE, CapabilityValue::Flag(true));
        assert_eq!(caps.iter().count(), 1);
        assert_eq!(caps.get(&HARDWARE), Some(&CapabilityValue::Flag(true)));
    }

    #[test]
    fn requirement_is_met_when_all_constraints_hold() {
        let req = Requirement::new()
            .with(CODECS, Constraint::Includes("hevc".into()))
            .with(WIDTH, Constraint::AtLeast(3840))
            .with(WIDTH, Constraint::AtMost(64))
            .with(HARDWARE, Constraint::Flag(true))
            .with(SESSIONS, Constraint::AtLeast(2));
        let caps = encoder_caps();
        assert!(req.is_satisfied_by(&caps));
        assert!(req.unmet(&caps).is_empty());
    }

    #[test]
    fn unmet_reports_each_failed_constraint() {
        let req = Requirement::new()
            .with(CODECS, Constraint::Includes("av1".into()))
            .with(WIDTH, Constraint::AtLeast(8192))
            .with(CapabilityKey::new("input.memory"), Constraint::Present);
        let caps = encoder_caps();
        assert!(!req.is_satisfied_by(&caps));

        let unmet = req.unmet(&caps);
        assert_eq!(unmet.len(), 3);
        assert_eq!(unmet[0].key, CODECS);
        assert!(unmet[2].found.is_none());
        assert!(unmet[2].to_string().contains("not declared"));
    }

    #[test]
    fn mismatched_value_kinds_never_match() {
        let req = Requirement::new().with(HARDWARE, Constraint::AtLeast(1));
        assert!(!req.is_satisfied_by(&encoder_caps()));
    }

    #[test]
    fn empty_requirement_matches_anything() {
        assert!(Requirement::new().is_satisfied_by(&CapabilitySet::new()));
    }
}
