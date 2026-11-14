//! Recording markers (ADR-0089 decisions 1 to 4).
//!
//! A [`MarkerLog`] belongs to one running recording. It holds the markers in memory
//! in the order they were added; its owner copies them into the recording manifest
//! ([`crate::RecordingManifest::markers`]) when it flushes and at stop. Adding a
//! marker is a plain call on the control thread: no node, no per-frame work and no
//! effect on any encoder (P13). Times are master-clock nanoseconds; a marker stores
//! its position relative to the recording's `t0`.

use std::fmt;
use std::time::Duration;

/// Most markers one recording keeps. One more is [`MarkerError::Full`].
pub const MAX_MARKERS: usize = 10_000;

/// Largest reaction-time offset (`marker_lead`, decision 4).
pub const MAX_MARKER_LEAD: Duration = Duration::from_secs(30);

/// Longest marker label, in characters.
pub const MAX_LABEL_CHARS: usize = 200;

/// What placed a marker.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum MarkerKind {
    /// The user (a shortcut, the Live controls button or `marker.add`).
    Manual,
    /// The program switched scenes; the label is the new scene's name.
    Scene,
    /// A recording protection event (ADR-0090), with its text.
    Protection,
    /// A stream started during the recording.
    StreamStart,
    /// A stream ended during the recording.
    StreamEnd,
}

impl MarkerKind {
    /// Every kind.
    pub const ALL: [Self; 5] = [
        Self::Manual,
        Self::Scene,
        Self::Protection,
        Self::StreamStart,
        Self::StreamEnd,
    ];

    /// The manifest name: `manual`, `scene`, `protection`, `stream_start` or
    /// `stream_end`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::Scene => "scene",
            Self::Protection => "protection",
            Self::StreamStart => "stream_start",
            Self::StreamEnd => "stream_end",
        }
    }

    /// Parses a manifest name.
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.as_str() == name)
    }
}

/// The fixed, semantic set of marker colours (decision 2). Only the app maps a
/// colour to a theme token; the data stores the name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum MarkerColor {
    /// The default.
    #[default]
    Blue,
    /// Red.
    Red,
    /// Orange.
    Orange,
    /// Yellow.
    Yellow,
    /// Green.
    Green,
    /// Purple.
    Purple,
}

impl MarkerColor {
    /// Every colour, in the order a picker shows them.
    pub const ALL: [Self; 6] = [
        Self::Blue,
        Self::Red,
        Self::Orange,
        Self::Yellow,
        Self::Green,
        Self::Purple,
    ];

    /// The manifest and control API name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Blue => "blue",
            Self::Red => "red",
            Self::Orange => "orange",
            Self::Yellow => "yellow",
            Self::Green => "green",
            Self::Purple => "purple",
        }
    }

    /// Parses a manifest or control API name.
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|c| c.as_str() == name)
    }
}

/// One marker of a recording.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Marker {
    /// 1-based, in the order the markers were added to the recording.
    pub index: u32,
    /// Position from `t0` in nanoseconds, after the reaction-time offset.
    pub t_ns: u64,
    /// For a manual marker, when it was pressed (from `t0`, before the offset).
    pub pressed_ns: Option<u64>,
    /// What placed it.
    pub kind: MarkerKind,
    /// One line of text. A manual marker without a label is "Marker N".
    pub label: String,
    /// Its colour.
    pub color: MarkerColor,
}

/// Why a marker was not added.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum MarkerError {
    /// The recording already has [`MAX_MARKERS`].
    Full,
    /// The label is longer than [`MAX_LABEL_CHARS`] characters.
    LabelTooLong,
    /// The label has a control character (a line break, a tab, ...).
    LabelControl,
    /// `marker_lead` is above [`MAX_MARKER_LEAD`].
    LeadTooLong,
}

impl fmt::Display for MarkerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Full => write!(f, "the recording has {MAX_MARKERS} markers already"),
            Self::LabelTooLong => {
                write!(f, "a marker label has at most {MAX_LABEL_CHARS} characters")
            }
            Self::LabelControl => f.write_str("a marker label is one line of text"),
            Self::LeadTooLong => write!(
                f,
                "the marker lead is at most {} seconds",
                MAX_MARKER_LEAD.as_secs()
            ),
        }
    }
}

impl std::error::Error for MarkerError {}

/// Checks a label: trimmed, one line, at most [`MAX_LABEL_CHARS`]. Blank is `None`.
pub fn checked_label(label: Option<&str>) -> Result<Option<String>, MarkerError> {
    let Some(label) = label.map(str::trim).filter(|l| !l.is_empty()) else {
        return Ok(None);
    };
    if label.chars().any(char::is_control) {
        return Err(MarkerError::LabelControl);
    }
    if label.chars().count() > MAX_LABEL_CHARS {
        return Err(MarkerError::LabelTooLong);
    }
    Ok(Some(label.to_owned()))
}

/// The markers of one running recording.
#[derive(Debug, Clone)]
pub struct MarkerLog {
    t0_ns: u64,
    lead_ns: u64,
    markers: Vec<Marker>,
    manual: u32,
    flushed: usize,
}

impl MarkerLog {
    /// A log for a recording that started at `t0_ns` on the master clock. `lead` moves
    /// manual markers earlier (0 to 30 seconds).
    pub fn new(t0_ns: u64, lead: Duration) -> Result<Self, MarkerError> {
        if lead > MAX_MARKER_LEAD {
            return Err(MarkerError::LeadTooLong);
        }
        // At most 30 s, so the nanoseconds fit in u64.
        let lead_ns = u64::try_from(lead.as_nanos()).unwrap_or(u64::MAX);
        Ok(Self {
            t0_ns,
            lead_ns,
            markers: Vec::new(),
            manual: 0,
            flushed: 0,
        })
    }

    /// The recording's `t0` on the master clock.
    pub fn t0_ns(&self) -> u64 {
        self.t0_ns
    }

    /// Adds a manual marker pressed at `now_ns` (master clock). A blank or absent
    /// label becomes "Marker N", N counting manual markers from 1. The position is
    /// `now - t0 - lead`, never before `t0`. A refused marker changes nothing.
    pub fn add_manual(
        &mut self,
        now_ns: u64,
        label: Option<&str>,
        color: Option<MarkerColor>,
    ) -> Result<&Marker, MarkerError> {
        let label = checked_label(label)?;
        self.room()?;
        let pressed = now_ns.saturating_sub(self.t0_ns);
        self.manual = self.manual.saturating_add(1);
        let label = label.unwrap_or_else(|| format!("Marker {}", self.manual));
        Ok(self.push(Marker {
            index: 0,
            t_ns: pressed.saturating_sub(self.lead_ns),
            pressed_ns: Some(pressed),
            kind: MarkerKind::Manual,
            label,
            color: color.unwrap_or_default(),
        }))
    }

    /// Adds an automatic marker at `now_ns`; the reaction-time offset does not apply.
    /// A blank label becomes the kind's name. [`MarkerKind::Manual`] is
    /// [`MarkerLog::add_manual`] with this label and colour.
    pub fn add_auto(
        &mut self,
        now_ns: u64,
        kind: MarkerKind,
        label: &str,
        color: MarkerColor,
    ) -> Result<&Marker, MarkerError> {
        if kind == MarkerKind::Manual {
            return self.add_manual(now_ns, Some(label), Some(color));
        }
        let label = checked_label(Some(label))?.unwrap_or_else(|| kind.as_str().to_owned());
        self.room()?;
        Ok(self.push(Marker {
            index: 0,
            t_ns: now_ns.saturating_sub(self.t0_ns),
            pressed_ns: None,
            kind,
            label,
            color,
        }))
    }

    /// Every marker, in the order added.
    pub fn markers(&self) -> &[Marker] {
        &self.markers
    }

    /// Whether markers were added since the last [`MarkerLog::mark_flushed`].
    pub fn has_pending(&self) -> bool {
        self.flushed < self.markers.len()
    }

    /// Records that every marker so far is in the manifest on disk.
    pub fn mark_flushed(&mut self) {
        self.flushed = self.markers.len();
    }

    fn room(&self) -> Result<(), MarkerError> {
        if self.markers.len() >= MAX_MARKERS {
            Err(MarkerError::Full)
        } else {
            Ok(())
        }
    }

    fn push(&mut self, mut marker: Marker) -> &Marker {
        // At most MAX_MARKERS (10 000), so the index fits in u32.
        marker.index = u32::try_from(self.markers.len() + 1).unwrap_or(u32::MAX);
        self.markers.push(marker);
        let last = self.markers.len() - 1;
        &self.markers[last]
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    const S: u64 = 1_000_000_000;

    #[test]
    fn manual_markers_count_from_one_and_keep_the_press_time() {
        let mut log = MarkerLog::new(10 * S, Duration::from_secs(2)).unwrap();
        let m = log.add_manual(15 * S, None, None).unwrap().clone();
        assert_eq!(
            m,
            Marker {
                index: 1,
                t_ns: 3 * S,
                pressed_ns: Some(5 * S),
                kind: MarkerKind::Manual,
                label: "Marker 1".to_owned(),
                color: MarkerColor::Blue,
            }
        );
        let m = log
            .add_manual(11 * S, Some("  Good take "), Some(MarkerColor::Green))
            .unwrap()
            .clone();
        assert_eq!(m.t_ns, 0, "never before t0");
        assert_eq!(m.label, "Good take");
        assert_eq!(m.index, 2);
        let m = log.add_manual(20 * S, Some("   "), None).unwrap();
        assert_eq!(m.label, "Marker 3");
    }

    #[test]
    fn automatic_markers_ignore_the_lead_and_do_not_count_as_manual() {
        let mut log = MarkerLog::new(S, Duration::from_secs(5)).unwrap();
        let m = log
            .add_auto(4 * S, MarkerKind::Scene, "Interview", MarkerColor::Purple)
            .unwrap();
        assert_eq!((m.index, m.t_ns, m.pressed_ns), (1, 3 * S, None));
        let m = log
            .add_auto(9 * S, MarkerKind::StreamStart, " ", MarkerColor::Red)
            .unwrap();
        assert_eq!(m.label, "stream_start");
        let m = log.add_manual(10 * S, None, None).unwrap();
        assert_eq!((m.index, m.label.as_str()), (3, "Marker 1"));
    }

    #[test]
    fn bad_labels_leads_and_a_full_log_are_refused() {
        assert_eq!(
            MarkerLog::new(0, Duration::from_secs(31)).err(),
            Some(MarkerError::LeadTooLong)
        );
        let mut log = MarkerLog::new(0, MAX_MARKER_LEAD).unwrap();
        assert_eq!(
            log.add_manual(S, Some("a\nb"), None).err(),
            Some(MarkerError::LabelControl)
        );
        let long = "x".repeat(MAX_LABEL_CHARS + 1);
        assert_eq!(
            log.add_manual(S, Some(&long), None).err(),
            Some(MarkerError::LabelTooLong)
        );
        assert!(log.markers().is_empty(), "a refused marker is not counted");
        for _ in 0..MAX_MARKERS {
            assert!(log.add_manual(S, None, None).is_ok());
        }
        assert_eq!(log.add_manual(S, None, None).err(), Some(MarkerError::Full));
        assert_eq!(
            log.add_auto(S, MarkerKind::Scene, "A", MarkerColor::Blue)
                .err(),
            Some(MarkerError::Full)
        );
    }

    #[test]
    fn pending_markers_clear_on_flush() {
        let mut log = MarkerLog::new(0, Duration::ZERO).unwrap();
        assert!(!log.has_pending());
        let _ = log.add_manual(S, None, None);
        assert!(log.has_pending());
        log.mark_flushed();
        assert!(!log.has_pending());
    }

    #[test]
    fn colour_and_kind_names_round_trip() {
        for c in MarkerColor::ALL {
            assert_eq!(MarkerColor::parse(c.as_str()), Some(c));
        }
        for k in MarkerKind::ALL {
            assert_eq!(MarkerKind::parse(k.as_str()), Some(k));
        }
        assert_eq!(MarkerColor::parse("pink"), None);
        assert_eq!(MarkerKind::parse("stream"), None);
    }
}
