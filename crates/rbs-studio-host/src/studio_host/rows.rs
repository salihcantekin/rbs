//! The window's rows for sources and their settings (ADR-0159 decision 8).

use super::*;

/// One source of the program scene, as the UI shows it.
#[derive(Debug, Clone)]
pub struct SourceRow {
    /// Its id in the collection.
    pub id: SourceId,
    /// Its name, unique in the collection.
    pub name: String,
    /// Its kind; `None` for a type this build does not know.
    pub kind: Option<SourceKind>,
    /// How it is doing.
    pub status: SourceStatus,
    /// Its level meter while it plays (audio sources).
    pub meter: Option<Arc<LevelMeter>>,
    /// Where its picture is on the canvas (video sources); `None` for sound only.
    pub layout: Option<Layout>,
    /// Where its picture is on the vertical canvas, in that canvas's pixels, while one
    /// exists (video sources, ADR-0093 decision 4): its own layout there, else the
    /// default one placed in that canvas.
    pub vertical_layout: Option<Layout>,
    /// Its item in the program scene.
    pub item: ItemId,
    /// The picture this source is the "Capture audio too" companion of (ADR-0096).
    pub companion_of: Option<SourceId>,
    /// Width and height in pixels of its picture, as last seen while it ran (video
    /// sources, ADR-0058); `None` before it ran.
    pub picture: Option<(u32, u32)>,
    /// Shown in the program scene. A hidden source is not drawn; its device stays
    /// open unless `active` is false.
    pub visible: bool,
    /// The device stays open while a scene that places the source is on air or in
    /// preview. False when the user deactivated it.
    pub active: bool,
    /// Locked: the picture panel does not select, move, resize or crop it (ADR-0059).
    pub locked: bool,
    /// The source's volume, a linear factor from 0 to 1, its mute (ADR-0059) and its
    /// sync offset (ADR-0062).
    pub audio: SourceAudio,
    /// What the mix hears while the program scene, in Own levels, has an entry for the
    /// source (ADR-0091); `None` when `audio` is what is heard.
    pub heard: Option<SourceAudio>,
    /// Its filters in chain order (ADR-0062).
    pub filters: Vec<filters::FilterRow>,
    /// A global source's row (ADR-0065): its item is in the canvas's global scene.
    pub global: bool,
    /// The other scenes that place the source, by name, in the Scenes panel's order;
    /// `in_globals` says whether it is also a global source.
    pub also_in: Vec<String>,
    /// The source is also a global source (on a scene row of a source that is both).
    pub in_globals: bool,
    /// The target id stored in its settings (`target`), empty when it has none: the
    /// Source tab marks it in the list (ADR-0077).
    pub target: String,
    /// Per-source slide override (ADR-0100); `None` uses the collection defaults.
    pub glide: Option<rbs_scene::SourceGlide>,
    /// A colour source's fill (`color`), empty for every other kind.
    pub fill: String,
    /// A colour source draws its fill. False leaves the inside empty. True for every
    /// other kind and when the setting is absent.
    pub filled: bool,
    /// A text source's stored settings (ADR-0129 decision 13: the Text tab shows
    /// them); `None` for every other kind.
    pub text: Option<Settings>,
    /// A browser source's stored settings (the Page tab shows them, ADR-0109,
    /// ADR-0132, ADR-0149); `None` for every other kind.
    pub browser: Option<Settings>,
    /// Why a browser source is not running, as the control API says it
    /// (`browser_runtime_missing`, `browser_runtime_mismatch`, `browser_cap`).
    pub browser_reason: Option<&'static str>,
    /// Capture region in full-picture pixels (ADR-0103); absent when `cap_has` is false.
    pub cap_has: bool,
    /// Left edge of the capture region.
    pub cap_l: u32,
    /// Top edge of the capture region.
    pub cap_t: u32,
    /// Right edge of the capture region.
    pub cap_r: u32,
    /// Bottom edge of the capture region.
    pub cap_b: u32,
    /// Display chrome (ADR-0104); off when `chrome_on` is false. `chrome_fill`: 0 cover,
    /// 1 fit.
    pub chrome_on: bool,
    /// Top chrome inset.
    pub chrome_t: u32,
    /// Right chrome inset.
    pub chrome_r: u32,
    /// Bottom chrome inset.
    pub chrome_b: u32,
    /// Left chrome inset.
    pub chrome_l: u32,
    /// Chrome fill: 0 cover, 1 fit.
    pub chrome_fill: u8,
    /// The listed display reported a work area for Reset chrome.
    pub has_work_rect: bool,
    /// Stored timed-show duration (ADR-0105); 0 when unset.
    pub show_for_ms: u32,
    /// Milliseconds left on a running Show cue; 0 when none.
    pub cue_remaining_ms: u32,
    /// A live pause holds the item's scroll run on the main canvas, then on the
    /// vertical one (ADR-0150 decision 12: Properties shows Resume).
    pub scroll_paused: [bool; 2],
    /// Which feeds are marked to record separately, and the picture's form (ADR-0086, ADR-0117).
    pub record_separately: RecordSeparately,
    /// The other sources of its audio device whose sync offset differs, by name
    /// (ADR-0126 decision 9); empty for every other source.
    pub device_peers: Vec<String>,
    /// How the latency of its running audio filters is compensated (ADR-0130 decision
    /// 4); `None` for a source the studio does not run.
    pub latency: Option<rbs_record::AudioLatency>,
    /// The stored playback settings of a media file (ADR-0108); `None` for every other
    /// kind. The play state and cursor travel in [`Update::Media`].
    pub media: Option<MediaSettings>,
    /// The policy in force for a source whose type offers a choice of `run`
    /// (ADR-0152); `None` for every other type, where Properties shows nothing.
    pub activity: Option<ActivityChoice>,
}

/// A source's activity policy in force, as Properties shows it (ADR-0152).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActivityChoice {
    /// The lowest level at which it runs.
    pub run: rbs_scene::activity::Run,
    /// What it does when it comes back; `None` for a type without one.
    pub on_return: Option<rbs_scene::activity::OnReturn>,
}

impl ActivityChoice {
    pub(super) fn of(source: &Source) -> Option<Self> {
        let traits = rbs_sources::activity_traits(&source.type_id);
        if traits.runs.len() < 2 {
            return None;
        }
        let effective = traits.effective(Some(&source.activity));
        Some(Self {
            run: effective.run,
            on_return: effective.on_return,
        })
    }
}

/// A media file's stored settings, as Properties shows them (ADR-0108).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaSettings {
    /// The file's path.
    pub path: String,
    /// Play again at the end (default on).
    pub looped: bool,
    /// Where the play range starts, in milliseconds of media time (default 0).
    pub start_ms: i64,
    /// Where the play range ends; `None` for the end of the file.
    pub end_ms: Option<i64>,
}

impl MediaSettings {
    pub(super) fn of(source: &Source) -> Option<Self> {
        if source.type_id.as_str() != types::MEDIA_SOURCE {
            return None;
        }
        let flag = |key: &str| !matches!(source.settings.get(key), Some(SettingValue::Bool(false)));
        Some(Self {
            path: match source.settings.get(keys::PATH) {
                Some(SettingValue::Text(path)) => path.clone(),
                _ => String::new(),
            },
            looped: flag(keys::LOOP),
            start_ms: match source.settings.get(keys::START_MS) {
                Some(SettingValue::Int(ms)) => (*ms).max(0),
                _ => 0,
            },
            end_ms: match source.settings.get(keys::END_MS) {
                Some(SettingValue::Int(ms)) => Some(*ms),
                _ => None,
            },
        })
    }
}

/// The play state of one media source for the UI (ADR-0108 decision 10): sent while
/// any plays, and when one changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MediaTick {
    /// The source.
    pub id: SourceId,
    /// 0 playing, 1 paused, 2 cued, 3 ended, 4 standby, 5 unavailable.
    pub state: u8,
    /// The cursor, in steps of [`MEDIA_TICK`].
    pub cursor_ms: u32,
    /// The file's length; 0 while unknown.
    pub duration_ms: u32,
}

/// How often the play state is sent while a media file plays.
pub(super) const MEDIA_TICK: Duration = Duration::from_millis(250);

impl PartialEq for SourceRow {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
            && self.name == other.name
            && self.kind == other.kind
            && self.status == other.status
            && self.layout == other.layout
            && self.vertical_layout == other.vertical_layout
            && self.item == other.item
            && self.companion_of == other.companion_of
            && self.record_separately == other.record_separately
            && self.device_peers == other.device_peers
            && self.latency == other.latency
            && self.media == other.media
            && self.activity == other.activity
            && self.picture == other.picture
            && self.visible == other.visible
            && self.active == other.active
            && self.locked == other.locked
            && self.audio == other.audio
            && self.heard == other.heard
            && self.filters == other.filters
            && self.global == other.global
            && self.also_in == other.also_in
            && self.in_globals == other.in_globals
            && self.target == other.target
            && self.glide == other.glide
            && self.fill == other.fill
            && self.filled == other.filled
            && self.text == other.text
            && self.browser == other.browser
            && self.browser_reason == other.browser_reason
            && self.cap_has == other.cap_has
            && self.cap_l == other.cap_l
            && self.cap_t == other.cap_t
            && self.cap_r == other.cap_r
            && self.cap_b == other.cap_b
            && self.chrome_on == other.chrome_on
            && self.chrome_t == other.chrome_t
            && self.chrome_r == other.chrome_r
            && self.chrome_b == other.chrome_b
            && self.chrome_l == other.chrome_l
            && self.chrome_fill == other.chrome_fill
            && self.has_work_rect == other.has_work_rect
            && self.show_for_ms == other.show_for_ms
            && self.cue_remaining_ms == other.cue_remaining_ms
            && self.scroll_paused == other.scroll_paused
            && match (&self.meter, &other.meter) {
                (Some(a), Some(b)) => Arc::ptr_eq(a, b),
                (None, None) => true,
                _ => false,
            }
    }
}
