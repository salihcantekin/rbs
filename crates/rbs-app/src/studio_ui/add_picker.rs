//! The Add source picker's data (ADR-0094 rule B1): what a source type can capture, or
//! the sources other scenes have, as rows a filter field narrows. The picker keeps its
//! own listing: the Source tab and Settings › Global sources ask the studio for target
//! lists too, and an answer of theirs never changes what the user is looking at here.
//! Each row says what it is (an icon, a detail line, a badge) and rows of one kind of
//! thing are grouped under a band (design system, "Searchable picker").

use rbs_app::studio_host::{SourceKind, Target};
use rbs_scene::model::SourceId;

use super::source_look::source_icon;

/// What a picked row adds.
#[derive(Debug, Clone, PartialEq)]
pub enum Pick {
    /// A device, window or screen of the picker's kind.
    Target(SourceKind, Target),
    /// A source another scene has.
    Existing(SourceId),
    /// A font family for a text source; empty is the system font (ADR-0129).
    Font(SourceId, String),
}

/// One row as the dialog shows it. `index` is the row's place in the picker's own
/// listing, whatever the filter hides, so choosing resolves against what was listed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub name: String,
    /// The detail line under the name; empty for none.
    pub note: String,
    pub index: usize,
    /// The icon of the row's tile (an icon name), shown until a picture comes.
    pub icon: &'static str,
    /// A short state on the right ("Main", "Default", "Current"); empty for none.
    pub badge: String,
    pub badge_icon: &'static str,
    /// The badge marks the current choice (accent fill).
    pub badge_accent: bool,
    /// The group this row starts (its band); empty when it continues the one above.
    pub group: String,
    /// How many bands are above this row's own.
    pub groups_before: usize,
    /// The family to draw the name in (fonts); empty for the UI font.
    pub font: String,
    /// The primary button's text while this row is highlighted.
    pub action: String,
    /// The capture target whose picture the row shows (displays and windows).
    pub picture: Option<(String, String)>,
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

/// One entry of the listing, as it was made from the answer.
#[derive(Debug, Clone)]
struct Entry {
    name: String,
    note: String,
    badge: &'static str,
    badge_icon: &'static str,
    badge_accent: bool,
    group: String,
    icon: &'static str,
    pick: Pick,
}

impl Entry {
    fn new(name: String, pick: Pick) -> Self {
        Self {
            name,
            note: String::new(),
            badge: "",
            badge_icon: "",
            badge_accent: false,
            group: String::new(),
            icon: "",
            pick,
        }
    }
}

/// A name short enough to stand in the primary button ("Add Display 1").
const ACTION_NAME_MAX: usize = 22;

#[derive(Debug)]
pub struct AddPicker {
    kind: Option<SourceKind>,
    /// The text source whose font is being chosen (the font list instead of a kind).
    font_for: Option<SourceId>,
    waiting: bool,
    entries: Vec<Entry>,
    query: String,
    /// "Capture audio too" is checked (displays and windows only, off at first).
    audio: bool,
}

impl AddPicker {
    /// A picker for what `kind` can capture; the rows come with [`Self::answer`].
    pub fn for_kind(kind: SourceKind) -> Self {
        Self {
            kind: Some(kind),
            font_for: None,
            waiting: true,
            entries: Vec::new(),
            query: String::new(),
            audio: false,
        }
    }

    /// A picker for the sources other scenes have (listed already), grouped by kind in
    /// the order the kinds are offered.
    pub fn existing(list: &[(SourceId, String, Option<SourceKind>)]) -> Self {
        let order = |k: &Option<SourceKind>| {
            k.and_then(|k| SourceKind::ALL.iter().position(|a| *a == k))
                .unwrap_or(usize::MAX)
        };
        let mut sorted: Vec<_> = list.iter().collect();
        sorted.sort_by_key(|(_, _, k)| order(k));
        let entries = sorted
            .into_iter()
            .map(|(id, name, kind)| {
                let label = kind.map_or("Other", |k| k.label());
                Entry {
                    note: label.to_owned(),
                    group: label.to_owned(),
                    icon: source_icon(*kind),
                    ..Entry::new(name.clone(), Pick::Existing(*id))
                }
            })
            .collect();
        Self {
            kind: None,
            font_for: None,
            waiting: false,
            entries,
            query: String::new(),
            audio: false,
        }
    }

    /// A picker for the font of the text source `source`; the families come with
    /// [`Self::answer_fonts`]. The current font is first under "Current", then "System
    /// font" (unless it is the current one) and the families under "All fonts".
    pub fn fonts(source: SourceId, current: &str) -> Self {
        let system = Entry {
            group: if current.is_empty() {
                "Current"
            } else {
                "All fonts"
            }
            .to_owned(),
            badge: if current.is_empty() { "Current" } else { "" },
            badge_accent: true,
            ..Entry::new("System font".to_owned(), Pick::Font(source, String::new()))
        };
        Self {
            kind: None,
            font_for: Some(source),
            waiting: true,
            entries: vec![system],
            query: String::new(),
            audio: false,
        }
    }

    /// The installed font families, sorted. Taken once, by a font picker only.
    pub fn answer_fonts(&mut self, families: &[String], current: &str) -> bool {
        let Some(source) = self.font_for.filter(|_| self.waiting) else {
            return false;
        };
        self.waiting = false;
        let family = |f: &String| Entry {
            group: "All fonts".to_owned(),
            ..Entry::new(f.clone(), Pick::Font(source, f.clone()))
        };
        if let Some(f) = families.iter().find(|f| *f == current) {
            self.entries.insert(
                0,
                Entry {
                    group: "Current".to_owned(),
                    badge: "Current",
                    badge_accent: true,
                    ..family(f)
                },
            );
        }
        self.entries
            .extend(families.iter().filter(|f| *f != current).map(family));
        true
    }

    /// The picker lists fonts.
    pub fn is_fonts(&self) -> bool {
        self.font_for.is_some()
    }

    /// The studio's listing of `kind`. Taken once, while the picker waits for its own
    /// kind; any other answer (another kind, a second answer) changes nothing. Windows
    /// are grouped by their application (in the order the applications first come).
    pub fn answer(&mut self, kind: SourceKind, list: &[Target]) -> bool {
        if !self.waiting || self.kind != Some(kind) {
            return false;
        }
        self.waiting = false;
        let mut entries: Vec<Entry> = list.iter().map(|t| target_entry(kind, t)).collect();
        if kind == SourceKind::Window {
            let mut apps: Vec<String> = Vec::new();
            for e in &entries {
                if !apps.contains(&e.group) {
                    apps.push(e.group.clone());
                }
            }
            entries.sort_by_key(|e| apps.iter().position(|a| *a == e.group));
        }
        self.entries = entries;
        // Cameras with a microphone of their own start with it checked (ADR-0096 E).
        self.audio = self.offers_microphone();
        true
    }

    /// Asks again ("Look again" after an empty answer): the picker waits for a new one.
    pub fn look_again(&mut self) -> Option<SourceKind> {
        let kind = self.kind?;
        self.waiting = true;
        self.entries.clear();
        Some(kind)
    }

    /// The dialog offers "Capture audio too": a display or a window is being added.
    pub fn offers_audio(&self) -> bool {
        matches!(self.kind, Some(SourceKind::Screen | SourceKind::Window))
            && self
                .entries
                .iter()
                .any(|e| matches!(&e.pick, Pick::Target(_, t) if t.sound && !t.picker))
    }

    /// The dialog offers "Add its microphone too": a camera is listed and at least one
    /// camera has a microphone that is not a source yet.
    pub fn offers_microphone(&self) -> bool {
        self.kind == Some(SourceKind::Camera)
            && self
                .entries
                .iter()
                .any(|e| matches!(&e.pick, Pick::Target(_, t) if t.mic.is_some()))
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

    /// The rows the filter keeps (case-insensitive, the name or detail contains the
    /// text). A row starts a band when its group differs from the row shown above it.
    pub fn rows(&self) -> Vec<Row> {
        let mut rows = Vec::new();
        let mut last: Option<&str> = None;
        let mut bands = 0;
        for (index, e) in self.entries.iter().enumerate() {
            if !self.query.is_empty()
                && !e.name.to_lowercase().contains(&self.query)
                && !(self.kind == Some(SourceKind::Window)
                    && e.note.to_lowercase().contains(&self.query))
            {
                continue;
            }
            let starts = !e.group.is_empty() && last != Some(e.group.as_str());
            last = Some(e.group.as_str());
            rows.push(Row {
                name: e.name.clone(),
                note: e.note.clone(),
                index,
                icon: e.icon,
                badge: e.badge.to_owned(),
                badge_icon: e.badge_icon,
                badge_accent: e.badge_accent,
                group: if starts {
                    e.group.clone()
                } else {
                    String::new()
                },
                groups_before: bands,
                font: match &e.pick {
                    Pick::Font(_, f) => f.clone(),
                    _ => String::new(),
                },
                action: self.action(&e.name),
                picture: match &e.pick {
                    Pick::Target(SourceKind::Screen | SourceKind::Window, t) if !t.picker => {
                        Some((t.adapter.clone(), t.target.clone()))
                    }
                    _ => None,
                },
            });
            if starts {
                bands += 1;
            }
        }
        rows
    }

    /// The primary button's text for a row named `name`: the name when it is short,
    /// else the kind of thing.
    fn action(&self, name: &str) -> String {
        let short = name.chars().count() <= ACTION_NAME_MAX;
        match (self.is_fonts(), self.kind) {
            (true, _) if short => format!("Use {name}"),
            (true, _) => "Use this font".to_owned(),
            (false, _) if short => format!("Add {name}"),
            (false, Some(k)) => format!("Add {}", what_of(k)),
            (false, None) => "Add source".to_owned(),
        }
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
        self.entries.get(index).map(|e| &e.pick)
    }

    /// The header: the kind's icon and one line that says what to pick.
    pub fn header(&self) -> (&'static str, &'static str) {
        if self.is_fonts() {
            return ("text", "The text is drawn in the font you pick.");
        }
        match self.kind {
            None => ("plus", "Place a source that other scenes already use."),
            Some(k) => (
                source_icon(Some(k)),
                match k {
                    SourceKind::Screen => "Choose the screen to capture.",
                    SourceKind::Window => "Choose the window to capture.",
                    SourceKind::Camera => "Choose the camera to add.",
                    SourceKind::AudioInput => "Choose the microphone or input to add.",
                    SourceKind::SystemAudio => "Choose the sound output to capture.",
                    SourceKind::ApplicationAudio => "Choose the application to hear.",
                    _ => "Choose what to add.",
                },
            ),
        }
    }
}

/// What a kind's targets are called in a sentence ("display", "window").
fn what_of(kind: SourceKind) -> &'static str {
    match kind {
        SourceKind::Screen => "display",
        SourceKind::Window => "window",
        SourceKind::Camera => "camera",
        SourceKind::AudioInput => "input",
        SourceKind::SystemAudio => "output",
        SourceKind::ApplicationAudio => "application",
        _ => "source",
    }
}

/// A target as a row: its size or application, and what makes it stand out.
fn target_entry(kind: SourceKind, t: &Target) -> Entry {
    let mut note: Vec<String> = Vec::new();
    if let Some((w, h)) = t.size {
        note.push(format!("{w} × {h}"));
    }
    let app = t.app.clone().filter(|a| !a.is_empty());
    if kind == SourceKind::Window
        && let Some(a) = &app
    {
        note.push(a.clone());
    }
    if t.mic.is_some() {
        note.push("Has a microphone".to_owned());
    }
    if t.recent.is_some() {
        note.push("Used recently".to_owned());
    }
    if t.picker {
        note.push("Opens the system's picker".to_owned());
    }
    let (badge, badge_icon) = match (&t.mic, t.is_default) {
        (Some(_), _) => ("Mic", "mic"),
        (None, true) if kind == SourceKind::Screen => ("Main", ""),
        (None, true) => ("Default", ""),
        (None, false) => ("", ""),
    };
    Entry {
        note: note.join(" · "),
        badge,
        badge_icon,
        group: if kind == SourceKind::Window {
            app.unwrap_or_else(|| "Other windows".to_owned())
        } else {
            String::new()
        },
        icon: source_icon(Some(kind)),
        ..Entry::new(t.name.clone(), Pick::Target(kind, t.clone()))
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
            sound: true,
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
            p.answer(kind, &[target("Thing", false)]);
            assert_eq!(p.offers_audio(), offered, "{kind:?}");
            assert!(!p.audio());
            p.set_audio(true);
            assert!(p.audio());
        }
        assert!(!AddPicker::existing(&[]).offers_audio());
    }

    #[test]
    fn a_display_that_cannot_leave_rbs_out_of_its_sound_does_not_offer_it() {
        let mut p = AddPicker::for_kind(SourceKind::Screen);
        let display = Target {
            sound: false,
            ..target("Display 1", false)
        };
        p.answer(SourceKind::Screen, &[display]);
        assert!(!p.offers_audio());
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
        assert_eq!(rows[0].badge, "Default");
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
        let p = AddPicker::existing(&[
            (
                SourceId::from_raw(4),
                "Mic".into(),
                Some(SourceKind::AudioInput),
            ),
            (
                SourceId::from_raw(5),
                "Logo".into(),
                Some(SourceKind::Image),
            ),
            (
                SourceId::from_raw(6),
                "Desk".into(),
                Some(SourceKind::Screen),
            ),
        ]);
        assert_eq!(p.status(), Status::Ready);
        assert_eq!(p.kind(), None);
        // Grouped by kind in the order kinds are offered, each with its kind's icon.
        let rows = p.rows();
        let shown: Vec<_> = rows
            .iter()
            .map(|r| (r.name.as_str(), r.group.as_str(), r.groups_before, r.icon))
            .collect();
        assert_eq!(
            shown,
            [
                ("Desk", "Screen capture", 0, "monitor"),
                ("Mic", "Audio input", 1, "mic"),
                ("Logo", "Image", 2, "image"),
            ]
        );
        assert_eq!(
            p.pick(rows[1].index),
            Some(&Pick::Existing(SourceId::from_raw(4)))
        );
        assert_eq!(rows[1].action, "Add Mic");
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
        assert_eq!(
            (p.rows()[0].badge.as_str(), p.rows()[0].badge_icon),
            ("Mic", "mic")
        );
        assert_eq!(p.rows()[0].note, "Has a microphone");
        assert_eq!(p.rows()[1].badge, "");
        // No camera has one: nothing is offered and nothing is checked.
        let mut none = AddPicker::for_kind(SourceKind::Camera);
        none.answer(SourceKind::Camera, &[cam("Built-in", None)]);
        assert!(!none.offers_microphone());
        assert!(!none.audio());
    }

    #[test]
    fn the_font_picker_puts_the_current_font_first_then_the_system_font() {
        let id = SourceId::from_raw(4);
        let mut p = AddPicker::fonts(id, "Inter");
        assert_eq!(p.status(), Status::Looking);
        assert!(p.is_fonts());
        // A device listing never fills it.
        assert!(!p.answer(SourceKind::Screen, &[target("Display", true)]));
        let families = vec!["Arial".to_owned(), "Inter".to_owned()];
        assert!(p.answer_fonts(&families, "Inter"));
        assert!(!p.answer_fonts(&families, "Inter"));
        let rows = p.rows();
        assert_eq!(
            rows.iter()
                .map(|r| (
                    r.name.as_str(),
                    r.badge.as_str(),
                    r.group.as_str(),
                    r.font.as_str()
                ))
                .collect::<Vec<_>>(),
            [
                ("Inter", "Current", "Current", "Inter"),
                ("System font", "", "All fonts", ""),
                ("Arial", "", "", "Arial"),
            ]
        );
        assert_eq!(p.pick(rows[1].index), Some(&Pick::Font(id, String::new())));
        assert_eq!(rows[0].action, "Use Inter");
        p.set_query("ari");
        let rows = p.rows();
        assert_eq!(
            (rows[0].name.as_str(), rows[0].group.as_str()),
            ("Arial", "All fonts")
        );
        // With the system font current, it is the current row.
        let mut sys = AddPicker::fonts(id, "");
        sys.answer_fonts(&families, "");
        let first = &sys.rows()[0];
        assert_eq!(
            (first.name.as_str(), first.badge.as_str()),
            ("System font", "Current")
        );
    }

    #[test]
    fn windows_are_grouped_by_their_application_and_displays_show_a_picture() {
        let win = |name: &str, app: &str| Target {
            app: Some(app.to_owned()),
            ..target(name, false)
        };
        let mut p = AddPicker::for_kind(SourceKind::Window);
        p.answer(
            SourceKind::Window,
            &[
                win("Docs", "Chrome"),
                win("Shell", "Terminal"),
                win("Mail", "Chrome"),
            ],
        );
        let rows = p.rows();
        let shown: Vec<_> = rows
            .iter()
            .map(|r| (r.name.as_str(), r.group.as_str(), r.groups_before))
            .collect();
        assert_eq!(
            shown,
            [
                ("Docs", "Chrome", 0),
                ("Mail", "", 1),
                ("Shell", "Terminal", 1)
            ]
        );
        assert_eq!(rows[0].picture, Some((String::new(), "Docs".to_owned())));
        // The application is searched too.
        p.set_query("term");
        assert_eq!(p.rows().len(), 1);
        let long = "A window title far too long for a button";
        let mut q = AddPicker::for_kind(SourceKind::Window);
        q.answer(SourceKind::Window, &[win(long, "App")]);
        assert_eq!(q.rows()[0].action, "Add window");
    }

    #[test]
    fn a_display_row_says_its_size_and_the_main_one() {
        let mut p = AddPicker::for_kind(SourceKind::Screen);
        let main = Target {
            size: Some((2560, 1440)),
            ..target("Display 1", true)
        };
        p.answer(SourceKind::Screen, &[main]);
        let r = &p.rows()[0];
        assert_eq!((r.note.as_str(), r.badge.as_str()), ("2560 × 1440", "Main"));
        assert_eq!(p.header(), ("monitor", "Choose the screen to capture."));
        // "Look again" waits for a new answer.
        assert_eq!(p.look_again(), Some(SourceKind::Screen));
        assert_eq!(p.status(), Status::Looking);
    }
}
