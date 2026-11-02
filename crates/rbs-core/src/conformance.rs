//! Shared support for the conformance suites of port crates: a report of named cases
//! and the common probe checks.
//!
//! Each port crate (`rbs-codec`, `rbs-mux`, `rbs-transport`, ...) exports suites
//! built on these types; adapter crates run them in their tests. This is test
//! support: it allocates freely and is never used on a runtime path. See
//! `docs/specs/backend-ports.md#conformance-tests`.

use std::fmt;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::time::{Duration, Instant};

use crate::capability::{CapabilityKey, CapabilitySet, CapabilityValue};
use crate::registry::AdapterFactory;

/// Longest a probe may take (backend ports spec).
pub const PROBE_BUDGET: Duration = Duration::from_millis(50);

/// The result of one case.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// All checks held.
    Passed,
    /// A check failed; the message says which.
    Failed(String),
    /// The case could not run here; the message says why.
    Skipped(String),
}

/// One named case of a suite.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Case {
    /// Case name, for example `h264/cbr/stream`.
    pub name: String,
    /// What happened.
    pub outcome: Outcome,
}

/// Everything a suite run found. Start with `Report::default()`.
#[non_exhaustive]
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    /// Cases in the order they ran.
    pub cases: Vec<Case>,
}

impl Report {
    /// Adds a case.
    pub fn record(&mut self, name: impl Into<String>, outcome: Outcome) {
        self.cases.push(Case {
            name: name.into(),
            outcome,
        });
    }

    /// Adds a case that passed when `result` is `Ok`.
    pub fn check(&mut self, name: impl Into<String>, result: Result<(), String>) {
        let outcome = match result {
            Ok(()) => Outcome::Passed,
            Err(e) => Outcome::Failed(e),
        };
        self.record(name, outcome);
    }

    /// The failed cases.
    pub fn failures(&self) -> impl Iterator<Item = &Case> {
        self.cases
            .iter()
            .filter(|c| matches!(c.outcome, Outcome::Failed(_)))
    }

    /// The skipped cases.
    pub fn skipped(&self) -> impl Iterator<Item = &Case> {
        self.cases
            .iter()
            .filter(|c| matches!(c.outcome, Outcome::Skipped(_)))
    }

    /// True when no case failed.
    pub fn is_success(&self) -> bool {
        self.failures().next().is_none()
    }

    /// Test helper: fails the calling test unless every named case ran and passed.
    ///
    /// Use it so an adapter that should work on the CI machine (software encoders,
    /// file transports) cannot pass by skipping everything.
    ///
    /// # Panics
    ///
    /// Panics with the report when a named case is missing, skipped or failed.
    pub fn assert_ran(&self, names: &[&str]) {
        self.assert_passed();
        for name in names {
            let passed = self
                .cases
                .iter()
                .any(|c| c.name == *name && c.outcome == Outcome::Passed);
            assert!(passed, "case {name} did not run and pass:\n{self}");
        }
    }

    /// Test helper: fails the calling test when any case failed.
    ///
    /// # Panics
    ///
    /// Panics with the list of failed cases. Meant for `#[test]` functions only.
    pub fn assert_passed(&self) {
        assert!(self.is_success(), "conformance failures:\n{self}");
    }
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for case in &self.cases {
            match &case.outcome {
                Outcome::Passed => writeln!(f, "  pass  {}", case.name)?,
                Outcome::Failed(e) => writeln!(f, "  FAIL  {}: {e}", case.name)?,
                Outcome::Skipped(e) => writeln!(f, "  skip  {}: {e}", case.name)?,
            }
        }
        Ok(())
    }
}

/// Runs `factory.probe()` as the `probe` case: no panic, within [`PROBE_BUDGET`].
///
/// Returns the capabilities when the adapter is usable here. A probe error or an
/// empty set is recorded as skipped (no hardware or library), not failed.
pub fn probe_case<F: AdapterFactory + ?Sized>(
    report: &mut Report,
    factory: &F,
) -> Option<CapabilitySet> {
    // Best of three, so one scheduling hiccup on a shared CI machine does not fail
    // the budget; a probe that is slow every time still does.
    let mut elapsed = Duration::MAX;
    let mut result = Err(());
    for _ in 0..3 {
        let started = Instant::now();
        result = catch_unwind(AssertUnwindSafe(|| factory.probe())).map_err(|_| ());
        elapsed = elapsed.min(started.elapsed());
        if elapsed <= PROBE_BUDGET || result.is_err() {
            break;
        }
    }
    match result {
        Err(_) => {
            report.record("probe", Outcome::Failed("probe panicked".into()));
            None
        }
        Ok(Err(e)) => {
            report.record(
                "probe",
                Outcome::Skipped(format!("adapter unavailable: {e}")),
            );
            None
        }
        Ok(Ok(caps)) => {
            report.check(
                "probe",
                if elapsed <= PROBE_BUDGET {
                    Ok(())
                } else {
                    Err(format!("probe took {elapsed:?}, budget {PROBE_BUDGET:?}"))
                },
            );
            if caps.is_empty() {
                report.record(
                    "capabilities",
                    Outcome::Skipped("no capabilities on this machine".into()),
                );
                return None;
            }
            Some(caps)
        }
    }
}

/// Runs `f`, turning a panic into an error, so one misbehaving adapter call fails a
/// case instead of the whole test.
pub fn guarded<T>(f: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
    catch_unwind(AssertUnwindSafe(f)).unwrap_or_else(|_| Err("adapter panicked".into()))
}

/// The symbols of a `Set` capability, or an empty list when the key is missing or not
/// a set.
pub fn set_values(caps: &CapabilitySet, key: &CapabilityKey) -> Vec<String> {
    match caps.get(key) {
        Some(CapabilityValue::Set(items)) => items.clone(),
        _ => Vec::new(),
    }
}

/// True when a `Flag` capability is declared `true`.
pub fn flag(caps: &CapabilitySet, key: &CapabilityKey) -> bool {
    caps.get(key) == Some(&CapabilityValue::Flag(true))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability::CapabilityKey;
    use crate::registry::{AdapterId, ProbeError};

    struct Probe(Result<CapabilitySet, ProbeError>, bool);

    impl AdapterFactory for Probe {
        fn id(&self) -> AdapterId {
            AdapterId::new("test.probe")
        }
        fn display_name(&self) -> &str {
            "probe"
        }
        fn probe(&self) -> Result<CapabilitySet, ProbeError> {
            if self.1 {
                panic!("probe panic");
            }
            self.0.clone()
        }
    }

    const KEY: CapabilityKey = CapabilityKey::new_static("test.key");

    #[test]
    fn probe_outcomes() {
        let caps = CapabilitySet::new().with(KEY, CapabilityValue::Flag(true));
        let mut report = Report::default();
        assert!(probe_case(&mut report, &Probe(Ok(caps), false)).is_some());
        assert!(probe_case(&mut report, &Probe(Ok(CapabilitySet::new()), false)).is_none());
        assert!(probe_case(&mut report, &Probe(Err(ProbeError::new("x")), false)).is_none());
        assert!(report.is_success(), "{report}");
        assert_eq!(report.skipped().count(), 2);

        let mut report = Report::default();
        assert!(probe_case(&mut report, &Probe(Ok(CapabilitySet::new()), true)).is_none());
        assert_eq!(report.failures().count(), 1);
    }

    #[test]
    fn capability_helpers() {
        let caps = CapabilitySet::new()
            .with(KEY, CapabilityValue::Set(vec!["a".into()]))
            .with(CapabilityKey::new("test.flag"), CapabilityValue::Flag(true));
        assert_eq!(set_values(&caps, &KEY), ["a"]);
        assert!(flag(&caps, &CapabilityKey::new("test.flag")));
        assert!(!flag(&caps, &KEY));
        assert_eq!(
            guarded::<()>(|| panic!("boom")),
            Err("adapter panicked".into())
        );
    }
}
