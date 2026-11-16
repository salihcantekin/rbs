//! The import report (ADR-0131 decision 11): one entry per OBS object that was not
//! imported exactly, and counts of what was.
//!
//! Reasons are one plain English sentence. A report never holds a credential, a file's
//! contents or a full path: paths go through [`file_name`].

use std::fmt;

/// What happened to one OBS object.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Outcome {
    /// Imported, but not exactly as OBS shows it.
    Approximated,
    /// Not imported.
    Skipped,
    /// Left out because the user did not choose it in the wizard.
    NotChosen,
}

impl Outcome {
    /// `"approximated"`, `"skipped"` or `"not-chosen"`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Approximated => "approximated",
            Self::Skipped => "skipped",
            Self::NotChosen => "not-chosen",
        }
    }
}

/// Which part of the OBS setup an entry is about.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Area {
    /// A scene or group.
    Scene,
    /// A source.
    Source,
    /// A filter on a source or scene.
    Filter,
    /// One placement of a source in a scene.
    Item,
    /// A transition.
    Transition,
    /// A hotkey.
    Hotkey,
    /// The output profile.
    Profile,
    /// Any other setting.
    Setting,
}

impl Area {
    /// The lowercase name.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Scene => "scene",
            Self::Source => "source",
            Self::Filter => "filter",
            Self::Item => "item",
            Self::Transition => "transition",
            Self::Hotkey => "hotkey",
            Self::Profile => "profile",
            Self::Setting => "setting",
        }
    }
}

/// One OBS object that was not imported exactly.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReportEntry {
    /// What happened.
    pub outcome: Outcome,
    /// Which part of the setup.
    pub area: Area,
    /// The object's OBS name (a source, scene or filter name, or a setting key).
    pub obs_name: String,
    /// The object's OBS type id (`image_source`, `noise_gate_filter`, ...); empty when
    /// the entry is about a setting.
    pub obs_type: String,
    /// Scenes the object is placed in, sorted, when that helps to find it.
    pub scenes: Vec<String>,
    /// One plain sentence: why.
    pub reason: String,
    /// One plain sentence: what the user can do, if anything.
    pub hint: Option<String>,
}

impl ReportEntry {
    /// An entry with no scenes and no hint.
    pub fn new(
        outcome: Outcome,
        area: Area,
        obs_name: impl Into<String>,
        obs_type: impl Into<String>,
        reason: impl Into<String>,
    ) -> Self {
        Self {
            outcome,
            area,
            obs_name: obs_name.into(),
            obs_type: obs_type.into(),
            scenes: Vec::new(),
            reason: reason.into(),
            hint: None,
        }
    }

    /// The same entry with this hint.
    #[must_use]
    pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }
}

impl fmt::Display for ReportEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} {} {:?}",
            self.outcome.as_str(),
            self.area.as_str(),
            self.obs_name
        )?;
        if !self.obs_type.is_empty() {
            write!(f, " ({})", self.obs_type)?;
        }
        if !self.scenes.is_empty() {
            write!(f, " in {}", self.scenes.join(", "))?;
        }
        write!(f, ": {}", self.reason)?;
        if let Some(h) = &self.hint {
            write!(f, " {h}")?;
        }
        Ok(())
    }
}

/// How many objects of each kind came across exactly.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Counts {
    /// Scenes.
    pub scenes: usize,
    /// Sources.
    pub sources: usize,
    /// Items placed in scenes.
    pub items: usize,
    /// Filters.
    pub filters: usize,
}

/// The whole report.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ImportReport {
    /// Objects imported exactly.
    pub exact: Counts,
    /// Objects that were not, in the order they were met.
    pub entries: Vec<ReportEntry>,
}

impl ImportReport {
    /// Adds an entry.
    pub fn push(&mut self, entry: ReportEntry) {
        self.entries.push(entry);
    }

    /// Entries with this outcome.
    pub fn with_outcome(&self, outcome: Outcome) -> impl Iterator<Item = &ReportEntry> {
        self.entries.iter().filter(move |e| e.outcome == outcome)
    }
}

impl fmt::Display for ImportReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let c = self.exact;
        writeln!(
            f,
            "Imported exactly: {} scenes, {} sources, {} items, {} filters.",
            c.scenes, c.sources, c.items, c.filters
        )?;
        for outcome in [Outcome::Approximated, Outcome::Skipped, Outcome::NotChosen] {
            let n = self.with_outcome(outcome).count();
            if n == 0 {
                continue;
            }
            let title = match outcome {
                Outcome::Approximated => "Imported, not exactly as in OBS",
                Outcome::Skipped => "Not imported",
                Outcome::NotChosen => "Not chosen",
            };
            writeln!(f, "{title} ({n}):")?;
            for e in self.with_outcome(outcome) {
                writeln!(f, "- {e}")?;
            }
        }
        Ok(())
    }
}

/// The last component of a path written by OBS on any operating system, for a report:
/// never the folders, which can name the user.
pub fn file_name(path: &str) -> &str {
    path.rsplit(['/', '\\']).next().unwrap_or(path)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.
mod tests {
    use super::*;

    #[test]
    fn file_names_drop_every_folder() {
        assert_eq!(file_name("C:\\Users\\me\\Pictures\\logo.png"), "logo.png");
        assert_eq!(file_name("/Users/me/logo.png"), "logo.png");
        assert_eq!(file_name("logo.png"), "logo.png");
    }

    #[test]
    fn the_report_reads_as_sentences() {
        let mut r = ImportReport::default();
        r.exact.scenes = 2;
        r.push(
            ReportEntry::new(
                Outcome::Skipped,
                Area::Source,
                "Title",
                "text_ft2_source_v2",
                "Text sources are not in RBS yet.",
            )
            .with_hint("Add it again when text sources arrive."),
        );
        let text = r.to_string();
        assert!(text.starts_with("Imported exactly: 2 scenes"), "{text}");
        assert!(
            text.contains("- skipped source \"Title\" (text_ft2_source_v2): Text sources are not in RBS yet. Add it again"),
            "{text}"
        );
    }
}
