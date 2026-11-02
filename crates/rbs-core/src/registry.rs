//! Adapter factories and runtime selection by capability.
//!
//! Each port defines its own factory trait with [`AdapterFactory`] as a supertrait,
//! and the engine keeps one [`Registry`] per port, for example
//! `Registry<dyn VideoEncoderFactory>`. See `docs/specs/backend-ports.md`.

use std::fmt;
use std::sync::Arc;

use crate::capability::{CapabilityKey, CapabilitySet, CapabilityValue, Requirement, Unmet};

/// Well-known capability every adapter should declare: `true` for hardware-backed.
pub const HARDWARE: CapabilityKey = CapabilityKey::new_static("adapter.hardware");

/// A stable, unique, reverse-DNS style adapter id, for example `org.rbs.codec.nvenc`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AdapterId(String);

impl AdapterId {
    /// Creates an id. Ids must never change once an adapter is released, because user
    /// settings and preference orders refer to them.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// The id as a string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for AdapterId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Why probing an adapter failed. Probe failures never abort start-up; the adapter is
/// simply unavailable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeError {
    /// Human readable reason, shown in diagnostics.
    pub reason: String,
}

impl ProbeError {
    /// Creates a probe error with a reason.
    pub fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
        }
    }
}

impl fmt::Display for ProbeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.reason)
    }
}

impl std::error::Error for ProbeError {}

/// Common part of every adapter factory, whatever its port.
pub trait AdapterFactory: Send + Sync + 'static {
    /// Stable unique id.
    fn id(&self) -> AdapterId;

    /// Human readable name for UIs and logs.
    fn display_name(&self) -> &str;

    /// Discovers what this adapter can do on this machine right now.
    ///
    /// Must be fast (under 50 ms) and must not panic when hardware or libraries are
    /// missing: return an empty set or a [`ProbeError`] instead.
    fn probe(&self) -> Result<CapabilitySet, ProbeError>;
}

/// Why an adapter was not offered for a requirement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RejectReason {
    /// Probing failed.
    ProbeFailed(ProbeError),
    /// The adapter has not been probed yet.
    NotProbed,
    /// The probed capabilities do not meet the requirement.
    Unmet(Vec<Unmet>),
}

impl fmt::Display for RejectReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RejectReason::ProbeFailed(e) => write!(f, "probe failed: {e}"),
            RejectReason::NotProbed => f.write_str("not probed"),
            RejectReason::Unmet(unmet) => {
                f.write_str("unmet: ")?;
                for (i, u) in unmet.iter().enumerate() {
                    if i > 0 {
                        f.write_str("; ")?;
                    }
                    write!(f, "{u}")?;
                }
                Ok(())
            }
        }
    }
}

/// The result of a selection: usable candidates in order of preference, plus the
/// adapters that were rejected and why.
pub struct Selection<F: ?Sized> {
    /// Adapters that satisfy the requirement, best first. Try them in order and fall
    /// back to the next one if creation fails.
    pub candidates: Vec<Arc<F>>,
    /// Adapters that do not satisfy the requirement, with reasons, for diagnostics.
    pub rejected: Vec<(AdapterId, RejectReason)>,
}

impl<F: ?Sized> Selection<F> {
    /// The best candidate, if any.
    pub fn best(&self) -> Option<&Arc<F>> {
        self.candidates.first()
    }
}

struct Entry<F: ?Sized> {
    id: AdapterId,
    factory: Arc<F>,
    probed: Option<Result<CapabilitySet, ProbeError>>,
}

/// Holds the adapters of one port and selects among them by capability.
///
/// Candidates are ordered by:
/// 1. the user's explicit choice, if set;
/// 2. the configured preference order;
/// 3. hardware adapters ([`HARDWARE`] = `true`) before software ones;
/// 4. registration order.
pub struct Registry<F: ?Sized + AdapterFactory> {
    entries: Vec<Entry<F>>,
    preference: Vec<AdapterId>,
    user_choice: Option<AdapterId>,
}

impl<F: ?Sized + AdapterFactory> Default for Registry<F> {
    fn default() -> Self {
        Self {
            entries: Vec::new(),
            preference: Vec::new(),
            user_choice: None,
        }
    }
}

/// Returned when registering an adapter whose id is already registered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DuplicateAdapter(pub AdapterId);

impl fmt::Display for DuplicateAdapter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "adapter {} is already registered", self.0)
    }
}

impl std::error::Error for DuplicateAdapter {}

impl<F: ?Sized + AdapterFactory> Registry<F> {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers an adapter. Ids must be unique within a registry.
    pub fn register(&mut self, factory: Arc<F>) -> Result<(), DuplicateAdapter> {
        let id = factory.id();
        if self.entries.iter().any(|e| e.id == id) {
            return Err(DuplicateAdapter(id));
        }
        self.entries.push(Entry {
            id,
            factory,
            probed: None,
        });
        Ok(())
    }

    /// Sets the preference order (typically shipped as per-platform data and adjustable
    /// by users). Ids not listed keep their default order after the listed ones.
    pub fn set_preference_order(&mut self, order: Vec<AdapterId>) {
        self.preference = order;
    }

    /// Sets or clears the adapter the user explicitly picked. It is still only offered
    /// when it satisfies the requirement.
    pub fn set_user_choice(&mut self, choice: Option<AdapterId>) {
        self.user_choice = choice;
    }

    /// Probes every adapter and caches the results. Call at start-up and again when
    /// devices change. A panicking probe is caught and recorded as a probe failure.
    pub fn probe_all(&mut self) {
        for entry in &mut self.entries {
            let factory = &entry.factory;
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| factory.probe()))
                .unwrap_or_else(|_| Err(ProbeError::new("probe panicked")));
            entry.probed = Some(result);
        }
    }

    /// The cached capabilities of an adapter, if it was probed successfully.
    pub fn capabilities(&self, id: &AdapterId) -> Option<&CapabilitySet> {
        self.entries
            .iter()
            .find(|e| &e.id == id)
            .and_then(|e| e.probed.as_ref())
            .and_then(|r| r.as_ref().ok())
    }

    /// Ids of all registered adapters in registration order.
    pub fn ids(&self) -> impl Iterator<Item = &AdapterId> {
        self.entries.iter().map(|e| &e.id)
    }

    /// Selects adapters that satisfy `requirement`, best first.
    pub fn select(&self, requirement: &Requirement) -> Selection<F> {
        let mut accepted: Vec<(usize, &Entry<F>)> = Vec::new();
        let mut rejected = Vec::new();

        for (index, entry) in self.entries.iter().enumerate() {
            match &entry.probed {
                None => rejected.push((entry.id.clone(), RejectReason::NotProbed)),
                Some(Err(e)) => {
                    rejected.push((entry.id.clone(), RejectReason::ProbeFailed(e.clone())));
                }
                Some(Ok(caps)) => {
                    let unmet = requirement.unmet(caps);
                    if unmet.is_empty() {
                        accepted.push((index, entry));
                    } else {
                        rejected.push((entry.id.clone(), RejectReason::Unmet(unmet)));
                    }
                }
            }
        }

        accepted.sort_by_key(|(index, entry)| self.rank(*index, entry));

        Selection {
            candidates: accepted
                .into_iter()
                .map(|(_, e)| Arc::clone(&e.factory))
                .collect(),
            rejected,
        }
    }

    /// Sort key: lower is better.
    fn rank(&self, index: usize, entry: &Entry<F>) -> (u8, usize, u8, usize) {
        let user = u8::from(self.user_choice.as_ref() != Some(&entry.id));
        let preference = self
            .preference
            .iter()
            .position(|id| id == &entry.id)
            .unwrap_or(usize::MAX);
        let hardware = entry
            .probed
            .as_ref()
            .and_then(|r| r.as_ref().ok())
            .and_then(|caps| caps.get(&HARDWARE))
            .map_or(1, |v| u8::from(v != &CapabilityValue::Flag(true)));
        (user, preference, hardware, index)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability::Constraint;

    const CODECS: CapabilityKey = CapabilityKey::new_static("codec.video.encode");

    /// A stand-in for a port-specific factory trait.
    trait EncoderFactory: AdapterFactory {}

    struct Fake {
        id: &'static str,
        hardware: bool,
        codecs: &'static [&'static str],
        fail: bool,
    }

    impl AdapterFactory for Fake {
        fn id(&self) -> AdapterId {
            AdapterId::new(self.id)
        }
        fn display_name(&self) -> &str {
            self.id
        }
        fn probe(&self) -> Result<CapabilitySet, ProbeError> {
            if self.fail {
                return Err(ProbeError::new("driver missing"));
            }
            Ok(CapabilitySet::new()
                .with(HARDWARE, CapabilityValue::Flag(self.hardware))
                .with(
                    CODECS,
                    CapabilityValue::Set(self.codecs.iter().map(|c| c.to_string()).collect()),
                ))
        }
    }

    impl EncoderFactory for Fake {}

    struct Panics;

    impl AdapterFactory for Panics {
        fn id(&self) -> AdapterId {
            AdapterId::new("test.panics")
        }
        fn display_name(&self) -> &str {
            "panics"
        }
        fn probe(&self) -> Result<CapabilitySet, ProbeError> {
            panic!("broken adapter")
        }
    }

    impl EncoderFactory for Panics {}

    fn fake(
        id: &'static str,
        hardware: bool,
        codecs: &'static [&'static str],
    ) -> Arc<dyn EncoderFactory> {
        Arc::new(Fake {
            id,
            hardware,
            codecs,
            fail: false,
        })
    }

    fn registry() -> Registry<dyn EncoderFactory> {
        let mut r: Registry<dyn EncoderFactory> = Registry::new();
        r.register(fake("test.software", false, &["h264", "av1"]))
            .ok();
        r.register(fake("test.hw-a", true, &["h264", "hevc"])).ok();
        r.register(fake("test.hw-b", true, &["h264", "hevc", "av1"]))
            .ok();
        r.register(Arc::new(Fake {
            id: "test.broken",
            hardware: true,
            codecs: &[],
            fail: true,
        }))
        .ok();
        r.probe_all();
        r
    }

    fn ids(selection: &Selection<dyn EncoderFactory>) -> Vec<String> {
        selection
            .candidates
            .iter()
            .map(|f| f.id().as_str().to_owned())
            .collect()
    }

    fn h264() -> Requirement {
        Requirement::new().with(CODECS, Constraint::Includes("h264".into()))
    }

    #[test]
    fn hardware_is_preferred_then_registration_order() {
        let selection = registry().select(&h264());
        assert_eq!(ids(&selection), ["test.hw-a", "test.hw-b", "test.software"]);
    }

    #[test]
    fn preference_order_overrides_hardware_default() {
        let mut r = registry();
        r.set_preference_order(vec![
            AdapterId::new("test.software"),
            AdapterId::new("test.hw-b"),
        ]);
        assert_eq!(
            ids(&r.select(&h264())),
            ["test.software", "test.hw-b", "test.hw-a"]
        );
    }

    #[test]
    fn user_choice_wins_only_when_it_satisfies_the_requirement() {
        let mut r = registry();
        r.set_user_choice(Some(AdapterId::new("test.software")));
        assert_eq!(ids(&r.select(&h264()))[0], "test.software");

        let hevc = Requirement::new().with(CODECS, Constraint::Includes("hevc".into()));
        assert_eq!(ids(&r.select(&hevc)), ["test.hw-a", "test.hw-b"]);
    }

    #[test]
    fn rejected_adapters_carry_reasons() {
        let av1 = Requirement::new().with(CODECS, Constraint::Includes("av1".into()));
        let selection = registry().select(&av1);
        assert_eq!(ids(&selection), ["test.hw-b", "test.software"]);
        assert_eq!(selection.rejected.len(), 2);
        let broken = selection
            .rejected
            .iter()
            .find(|(id, _)| id.as_str() == "test.broken")
            .map(|(_, reason)| reason.to_string());
        assert_eq!(broken.as_deref(), Some("probe failed: driver missing"));
    }

    #[test]
    fn unprobed_adapters_are_not_offered() {
        let mut r: Registry<dyn EncoderFactory> = Registry::new();
        r.register(fake("test.hw-a", true, &["h264"])).ok();
        let selection = r.select(&h264());
        assert!(selection.best().is_none());
        assert_eq!(selection.rejected[0].1, RejectReason::NotProbed);
    }

    #[test]
    fn duplicate_ids_are_refused() {
        let mut r: Registry<dyn EncoderFactory> = Registry::new();
        assert!(r.register(fake("test.x", true, &[])).is_ok());
        assert_eq!(
            r.register(fake("test.x", false, &[])),
            Err(DuplicateAdapter(AdapterId::new("test.x")))
        );
    }

    #[test]
    fn panicking_probe_is_contained() {
        let mut r: Registry<dyn EncoderFactory> = Registry::new();
        r.register(Arc::new(Panics)).ok();
        r.probe_all();
        let selection = r.select(&Requirement::new());
        assert!(selection.candidates.is_empty());
        assert_eq!(
            selection.rejected[0].1,
            RejectReason::ProbeFailed(ProbeError::new("probe panicked"))
        );
    }
}
