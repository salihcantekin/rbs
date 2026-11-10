//! The Add source picker's data (ADR-0094 rule B1): what a source type can capture, or
//! the sources other scenes have, as rows a filter field narrows. The picker keeps its
//! own listing: the Source tab and Settings › Global sources ask the studio for target
//! lists too, and an answer of theirs never changes what the user is looking at here.

use rbs_app::studio_host::{SourceKind, Target};
use rbs_scene::model::SourceId;

/// What a picked row adds.
#[derive(Debug, Clone, PartialEq)]
pub enum Pick {
    /// A device, window or screen of the picker's kind.
    Target(SourceKind, Target),
    /// A source another scene has.
    Existing(SourceId),
}

/// One row as the dialog shows it. `index` is the row's place in the picker's own
/// listing, whatever the filter hides, so choosing resolves against what was listed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub name: String,
    pub note: String,
    pub index: usize,
}

/// What the status line says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// The studio has not answered yet.
    Looking,
    /// The answer was empty.
    Nothing,
    /// The filter hides every row.
    NoMatch,
    /// Rows are shown.
    Ready,
}

#[derive(Debug)]
pub struct AddPicker {
    kind: Option<SourceKind>,
    waiting: bool,
    entries: Vec<(String, String, Pick)>,
    query: String,
    /// "Capture audio too" is checked (displays and windows only, off at first).
    audio: bool,
}

impl AddPicker {
    /// A picker for what `kind` can capture; the rows come with [`Self::answer`].
    pub fn for_kind(kind: SourceKind) -> Self {
        Self {
            kind: Some(kind),
            waiting: true,
            entries: Vec::new(),
            query: String::new(),
            audio: false,
        }
    }

    /// A picker for the sources other scenes have (listed already).
    pub fn existing(list: &[(SourceId, String)]) -> Self {
        Self {
            kind: None,
            waiting: false,
            entries: list
                .iter()
                .map(|(id, name)| (name.clone(), String::new(), Pick::Existing(*id)))
                .collect(),
            query: String::new(),
            audio: false,
        }
    }

    /// The studio's listing of `kind`. Taken once, while the picker waits for its own
    /// kind; any other answer (another kind, a second answer) changes nothing.
    pub fn answer(&mut self, kind: SourceKind, list: &[Target]) -> bool {
        if !self.waiting || self.kind != Some(kind) {
            return false;
        }
        self.waiting = false;
        self.entries = list
            .iter()
            .map(|t| {
                let note = match (&t.mic, t.is_default) {
                    (Some(mic), _) => format!("with {mic}"),
                    (None, true) => "default".to_owned(),
                    (None, false) => String::new(),
                };
                (
                    t.name.clone(),
                    note,
                    Pick::Target(kind, t.clone()),
                )
            })
            .collect();
        // Cameras with a microphone of their own start with it checked (ADR-0096 E).
        self.audio = self.offers_microphone();
        true
    }

    /// The dialog offers "Capture audio too": a display or a window is being added.
    pub fn offers_audio(&self) -> bool {
        matches!(self.kind, Some(SourceKind::Screen | SourceKind::Window))
    }

    /// The dialog offers "Add its microphone too": a camera is listed and at least one
    /// camera has a microphone that is not a source yet.
    pub fn offers_microphone(&self) -> bool {
        self.kind == Some(SourceKind::Camera)
            && self
                .entries
                .iter()
                .any(|(_, _, p)| matches!(p, Pick::Target(_, t) if t.mic.is_some()))
    }

    /// "Capture audio too" is checked.
    pub fn audio(&self) -> bool {
        self.audio
    }

    /// Checks or unchecks "Capture audio too".
    pub fn set_audio(&mut self, on: bool) {
        self.audio = on;
    }

    pub fn kind(&self) -> Option<SourceKind> {
        self.kind
    }

    pub fn set_query(&mut self, text: &str) {
        self.query = text.trim().to_lowercase();
    }

    /// The rows the filter keeps (case-insensitive, the name contains the text).
    pub fn rows(&self) -> Vec<Row> {
        self.entries
            .iter()
            .enumerate()
            .filter(|(_, (name, _, _))| {
                self.query.is_empty() || name.to_lowercase().contains(&self.query)
            })
            .map(|(index, (name, note, _))| Row {
                name: name.clone(),
                note: note.clone(),
                index,
            })
            .collect()
    }

    pub fn status(&self) -> Status {
        if self.waiting {
            Status::Looking
        } else if self.entries.is_empty() {
            Status::Nothing
        } else if self.rows().is_empty() {
            Status::NoMatch
        } else {
            Status::Ready
        }
    }

    /// The row `index` of the listing (not of the filtered rows).
    pub fn pick(&self, index: usize) -> Option<&Pick> {
        self.entries.get(index).map(|(_, _, p)| p)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(name: &str, is_default: bool) -> Target {
        Target {
            name: name.into(),
            adapter: String::new(),
            target: name.into(),
            is_default,
            size: None,
            app: None,
            recent: None,
            picker: false,
            mic: None,
        }
    }

    #[test]
    fn capture_audio_too_is_offered_for_displays_and_windows_only_and_starts_off() {
        for (kind, offered) in [
            (SourceKind::Screen, true),
            (SourceKind::Window, true),
            (SourceKind::Camera, false),
            (SourceKind::AudioInput, false),
        ] {
            let mut p = AddPicker::for_kind(kind);
            assert_eq!(p.offers_audio(), offered, "{kind:?}");
            assert!(!p.audio());
            p.set_audio(true);
            assert!(p.audio());
        }
        assert!(!AddPicker::existing(&[]).offers_audio());
    }

    #[test]
    fn rows_keep_the_listings_index_when_the_filter_hides_some() {
        let mut p = AddPicker::for_kind(SourceKind::Camera);
        assert_eq!(p.status(), Status::Looking);
        let list = [
            target("Webcam", true),
            target("Capture card", false),
            target("Virtual cam", false),
        ];
        assert!(p.answer(SourceKind::Camera, &list));
        p.set_query(" CAM ");
        let rows = p.rows();
        assert_eq!(rows.iter().map(|r| r.index).collect::<Vec<_>>(), [0, 2]);
        assert_eq!(rows[0].note, "default");
        let Some(Pick::Target(_, t)) = p.pick(rows[1].index) else {
            panic!("a target");
        };
        assert_eq!(t.name, "Virtual cam");
        p.set_query("zzz");
        assert_eq!(p.status(), Status::NoMatch);
    }

    #[test]
    fn only_the_first_answer_of_its_own_kind_fills_the_picker() {
        let mut p = AddPicker::for_kind(SourceKind::Window);
        assert!(!p.answer(SourceKind::Screen, &[target("Display", true)]));
        assert_eq!(p.status(), Status::Looking);
        assert!(p.answer(SourceKind::Window, &[]));
        assert_eq!(p.status(), Status::Nothing);
        // The Source tab asking for the same kind later does not replace the rows.
        assert!(!p.answer(SourceKind::Window, &[target("Editor", false)]));
        assert!(p.rows().is_empty());
    }

    #[test]
    fn existing_sources_are_listed_at_once() {
        let p = AddPicker::existing(&[(SourceId::from_raw(4), "Mic".into())]);
        assert_eq!(p.status(), Status::Ready);
        assert_eq!(p.pick(0), Some(&Pick::Existing(SourceId::from_raw(4))));
        assert_eq!(p.kind(), None);
    }

    #[test]
    fn a_camera_with_a_microphone_offers_it_checked_and_says_so_in_its_row() {
        let cam = |name: &str, mic: Option<&str>| Target {
            mic: mic.map(str::to_owned),
            ..target(name, false)
        };
        let mut p = AddPicker::for_kind(SourceKind::Camera);
        p.answer(
            SourceKind::Camera,
            &[cam("Webcam", Some("Webcam mic")), cam("Built-in", None)],
        );
        assert!(p.offers_microphone());
        assert!(!p.offers_audio());
        assert!(p.audio());
        assert_eq!(p.rows()[0].note, "with Webcam mic");
        assert_eq!(p.rows()[1].note, "");
        // No camera has one: nothing is offered and nothing is checked.
        let mut none = AddPicker::for_kind(SourceKind::Camera);
        none.answer(SourceKind::Camera, &[cam("Built-in", None)]);
        assert!(!none.offers_microphone());
        assert!(!none.audio());
    }
}
