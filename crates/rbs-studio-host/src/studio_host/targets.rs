//! What a source picks: source kinds, capture targets and how a reopened window is found again.

use super::*;

/// A kind of source the user can add. The UI names them with glossary terms.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SourceKind {
    /// A whole display.
    Screen,
    /// One window.
    Window,
    /// A camera or capture card.
    Camera,
    /// A microphone or line input.
    AudioInput,
    /// What the computer plays.
    SystemAudio,
    /// What one application plays.
    ApplicationAudio,
    /// A picture: a file or a built-in sample (ADR-0074). Not in [`SourceKind::ALL`]:
    /// it has no target to list; the Library adds it.
    Image,
    /// A solid colour or a gradient (ADR-0074). Not in [`SourceKind::ALL`].
    Color,
    /// A local video or animated picture (ADR-0108). Not in [`SourceKind::ALL`]:
    /// added with a path like [`SourceKind::Image`].
    Media,
    /// A web page from the browser helper (ADR-0109). Not in [`SourceKind::ALL`].
    Browser,
    /// A text, drawn by the registered text renderer (ADR-0129). Not in
    /// [`SourceKind::ALL`]: it has no target; Add source adds "Text" at once.
    Text,
    /// The pictures of one folder, one after another (ADR-0171). Not in
    /// [`SourceKind::ALL`]: added with a folder path.
    Slideshow,
}

impl SourceKind {
    /// Every kind, in the order the add-source picker lists them.
    pub const ALL: [SourceKind; 6] = [
        SourceKind::Screen,
        SourceKind::Window,
        SourceKind::Camera,
        SourceKind::AudioInput,
        SourceKind::SystemAudio,
        SourceKind::ApplicationAudio,
    ];

    /// The source type of this kind.
    pub fn type_id(self) -> NodeTypeId {
        NodeTypeId::new_static(match self {
            SourceKind::Screen => types::DISPLAY_SOURCE,
            SourceKind::Window => types::WINDOW_SOURCE,
            SourceKind::Camera => types::CAMERA_SOURCE,
            SourceKind::AudioInput => types::AUDIO_INPUT_SOURCE,
            SourceKind::SystemAudio => types::AUDIO_OUTPUT_SOURCE,
            SourceKind::ApplicationAudio => types::APPLICATION_AUDIO_SOURCE,
            SourceKind::Image => types::IMAGE_SOURCE,
            SourceKind::Color => types::COLOR_SOURCE,
            SourceKind::Media => types::MEDIA_SOURCE,
            SourceKind::Browser => types::BROWSER_SOURCE,
            SourceKind::Text => types::TEXT_SOURCE,
            SourceKind::Slideshow => types::SLIDESHOW_SOURCE,
        })
    }

    /// The kind of a source type.
    pub fn of(type_id: &NodeTypeId) -> Option<SourceKind> {
        SourceKind::ALL
            .into_iter()
            .chain([
                SourceKind::Image,
                SourceKind::Color,
                SourceKind::Media,
                SourceKind::Browser,
                SourceKind::Text,
                SourceKind::Slideshow,
            ])
            .find(|k| k.type_id() == *type_id)
    }

    /// The source type's name in the UI (glossary: "Capture" only in type names).
    pub fn label(self) -> &'static str {
        match self {
            SourceKind::Screen => "Screen capture",
            SourceKind::Window => "Window capture",
            SourceKind::Camera => "Camera",
            SourceKind::AudioInput => "Audio input",
            SourceKind::SystemAudio => "System audio",
            SourceKind::ApplicationAudio => "Application audio",
            SourceKind::Image => "Image",
            SourceKind::Color => "Colour",
            SourceKind::Media => "Media file",
            SourceKind::Browser => "Browser",
            SourceKind::Text => "Text",
            SourceKind::Slideshow => "Slide show",
        }
    }

    /// The `kind` parameter of `source.add` / the control API.
    pub fn api_id(self) -> &'static str {
        match self {
            SourceKind::Screen => "screen",
            SourceKind::Window => "window",
            SourceKind::Camera => "camera",
            SourceKind::AudioInput => "audio_input",
            SourceKind::SystemAudio => "system_audio",
            SourceKind::ApplicationAudio => "application_audio",
            SourceKind::Image => "image",
            SourceKind::Color => "color",
            SourceKind::Media => "media",
            SourceKind::Browser => "browser",
            SourceKind::Text => "text",
            SourceKind::Slideshow => "slideshow",
        }
    }

    /// True for kinds that only make sound.
    pub fn is_audio(self) -> bool {
        matches!(
            self,
            SourceKind::AudioInput | SourceKind::SystemAudio | SourceKind::ApplicationAudio
        )
    }

    /// True when Add source opens a file or URL dialog instead of a device list.
    pub fn picks_path(self) -> bool {
        matches!(self, SourceKind::Media | SourceKind::Image)
    }
}

/// The one listed window a window source's hints name: none while its stored target
/// is still listed, none when no or several windows have the same application
/// (case-insensitive) and title (exact).
pub(super) fn match_by_hints(
    listed: Vec<Target>,
    adapter: &str,
    stored: &str,
    app: &str,
    title: &str,
) -> Option<Target> {
    if listed
        .iter()
        .any(|t| t.adapter == adapter && t.target == stored)
    {
        return None;
    }
    let app = app.to_lowercase();
    let mut same = listed.into_iter().filter(|t| {
        t.adapter == adapter
            && t.name == title
            && t.app.as_deref().is_some_and(|a| a.to_lowercase() == app)
    });
    let found = same.next()?;
    same.next().is_none().then_some(found)
}

/// What a reload did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Reload {
    /// The source was re-pointed at the window its hints name: the collection changed.
    Retargeted,
    /// The source node was restarted; `true` when it was running to restart.
    Restarted(bool),
}

/// Whether two application names are the same program: equal ignoring ASCII case and
/// an optional `.exe` suffix (older sources saved `chrome.exe`, window capture says `chrome`).
pub(super) fn same_app(a: &str, b: &str) -> bool {
    fn stem(n: &str) -> &str {
        match n.len().checked_sub(4) {
            Some(i) if n.is_char_boundary(i) && n[i..].eq_ignore_ascii_case(".exe") => &n[..i],
            _ => n,
        }
    }
    stem(a).eq_ignore_ascii_case(stem(b))
}

#[cfg(test)]
mod same_app_tests {
    use super::same_app;

    #[test]
    fn ignores_case_and_exe_suffix() {
        assert!(same_app("chrome", "chrome.exe"));
        assert!(same_app("Chrome.EXE", "chrome"));
        assert!(same_app("Google Chrome", "google chrome"));
        assert!(!same_app("chrome", "chromium.exe"));
    }
}

/// The hint keys of a source (ADR-0077): the application and the title its target had.
pub(super) const HINT_APP: &str = "target_app";
pub(super) const HINT_TITLE: &str = "target_title";

/// The hints a source gets for `target`: its title always, its application when known.
pub(super) fn hints_of(target: &Target) -> BTreeMap<String, String> {
    let mut hints = BTreeMap::new();
    if !target.name.is_empty() {
        hints.insert(HINT_TITLE.to_owned(), target.name.clone());
    }
    if let Some(app) = target.app.as_ref().filter(|a| !a.is_empty()) {
        hints.insert(HINT_APP.to_owned(), app.clone());
    }
    hints
}

/// Something a source of a kind can capture: a display, window, camera or endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    /// Name from the OS.
    pub name: String,
    /// The adapter that listed it.
    pub adapter: String,
    /// Its id for that adapter.
    pub target: String,
    /// The OS default of its kind (audio endpoints).
    pub is_default: bool,
    /// Native size in pixels when known (displays and windows).
    pub size: Option<(u32, u32)>,
    /// The application that owns a window, when the OS tells it.
    pub app: Option<String>,
    /// Where the user picked it recently for this source type: 0 is the newest
    /// ([`crate::recent_targets`]); `None` when it is not among the recent targets.
    pub recent: Option<usize>,
    /// Not a target yet: adding it shows the OS picker of `adapter` (systems that
    /// list no screens or windows, such as the Linux screen-cast portal), and the
    /// target the user chooses is added.
    pub picker: bool,
    /// Name of the camera's own microphone, when the audio system has it and it is not
    /// a source yet (ADR-0096 E); `None` otherwise and for everything but cameras.
    pub mic: Option<String>,
    /// The sound that goes with a display or window can be added too (the system can
    /// leave RBS's own sound out of a display's sound); false when it cannot.
    pub sound: bool,
}

impl Target {
    /// A `source.targets` row as the window lists it (protocol 1.101 carries every
    /// field, ADR-0168).
    pub fn from_api(t: rbs_api::types::CaptureTarget) -> Self {
        Self {
            name: t.name,
            adapter: t.adapter,
            target: t.target,
            is_default: t.is_default,
            size: t.size.map(|d| (d.width, d.height)),
            app: t.app,
            recent: t.recent.map(|r| r as usize),
            picker: t.picker,
            mic: t.mic,
            sound: t.sound,
        }
    }
}

/// How a source is doing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceStatus {
    /// It runs.
    Running,
    /// It is not shown or played, so it is not opened (P13): for example a video source
    /// where no preview runs.
    Idle,
    /// It is not running, and why.
    Failed(String),
    /// It cannot deliver because its target is gone, refused or busy (ADR-0077); `detail`
    /// is the adapter's own text. The UI words the cause by `fault` and the source's kind.
    Unavailable {
        /// The typed cause.
        fault: rbs_core::fault::SourceFault,
        /// The adapter's text, for tooltips and logs.
        detail: String,
    },
}

/// A listed target as the UI shows it.
pub(super) fn target_of(t: SourceTarget) -> Target {
    Target {
        name: t.name,
        adapter: t.adapter.as_str().to_owned(),
        target: t.target,
        is_default: t.is_default,
        size: t.size,
        app: t.app,
        recent: None,
        picker: false,
        mic: None,
        sound: true,
    }
}

/// Where `target` is in the recent list (0 is the newest); pickers are never recent.
pub(super) fn recent_rank(recent: &[Recent], target: &Target) -> Option<usize> {
    if target.picker {
        return None;
    }
    recent
        .iter()
        .position(|r| r.adapter == target.adapter && r.target == target.target)
}

/// The entry that opens the OS picker for `kind` (glossary: screen, window).
pub(super) fn picker_name(kind: SourceKind) -> &'static str {
    match kind {
        SourceKind::Window => "Choose a window…",
        _ => "Choose a screen…",
    }
}

pub(super) fn kind_noun(kind: SourceKind) -> &'static str {
    match kind {
        SourceKind::Window => "window",
        _ => "screen",
    }
}
