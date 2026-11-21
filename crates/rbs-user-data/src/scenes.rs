//! The scene collection document (glossary: Scene collection): the on-disk form of
//! [`rbs_scene::SceneCollection`], mapping one-to-one onto the model (ADR-0021).
//!
//! ```toml
//! format = 15                 # 16 when a source records separately (ADR-0086), 17 when a scene has own levels (ADR-0091)
//! kind = "scene-collection"
//! name = "Main show"
//!
//! [next]                       # ids issued next, so ids are never reused
//! canvas = 1
//! source = 1
//! scene = 1
//! item = 1
//! filter = 0
//!
//! # [glide]                    # collection morph (format 12, ADR-0100); absent: 400 ms, ease-in-out
//! # duration_ms = 400
//! # easing = "ease-in-out"     # or "linear"
//!
//! # [transition]               # format 19, ADR-0128; absent: Move (format 18 and older: Cut when the glide is 0 ms)
//! # kind = "fade"              # "cut", "fade", "move", "swipe", "slide" or "stinger" (format 21)
//! # direct = "cut"             # the kind outside studio mode, same values (format 27, ADR-0153)
//! # fade = { duration_ms = 300, easing = "linear", via = "#000000" }   # via: fade through a colour
//! # swipe = { duration_ms = 500, easing = "ease-in-out", direction = "left" }  # "left", "right", "up", "down"
//! # slide = { duration_ms = 500, easing = "ease-in-out", direction = "left" }
//! # stinger = { source = 4, cut_ms = 700 }   # format 21, ADR-0140; each part optional
//!
//! [[canvases]]
//! id = 0
//! name = "Main"
//! width = 1920
//! height = 1080
//! frame_rate = [30, 1]         # numerator, denominator
//! program = 0                  # scene id; optional, like preview
//! # globals = 2                # the canvas's global scene (format 5, ADR-0065)
//! # master = { volume = 0.5, muted = true }   # the canvas's master (format 7, ADR-0081)
//! # follow = 1                  # shows the program scene of canvas 1 (format 9, ADR-0093)
//! # fallback = { scene = 3 }    # or "keep_last" (the default); only with `follow`
//!
//! [[sources]]
//! id = 0
//! name = "Display"
//! type = "org.rbs.source.display"
//! settings = { cursor = true }
//! # monitor = "also"         # heard on the monitor output: "only" or "also" (format 10, ADR-0097)
//! # glide = { duration_ms = 600, easing = "linear" }  # slide override (format 12, ADR-0100)
//! # region = [0, 40, 0, 80]   # full-picture insets, left top right bottom (format 14, ADR-0103)
//! # chrome = { top = 40, bottom = 80, fill = "cover" }  # display only (format 14, ADR-0104)
//! # record_separately = { video = true, audio = true }  # format 16, ADR-0086; absent: neither
//! # record_separately = { video = true, video_as = "image" }  # "image" or "video" (format 18, ADR-0117); absent: automatic
//! # activity = { run = "on_air", on_return = "resume", linger_s = 10 }  # each key optional; absent: the type's default (format 24, ADR-0152)
//!
//! [[scenes]]
//! id = 0
//! name = "Scene"
//! # canvas = 0                 # only that canvas shows it; absent: shared
//! # audio = { levels = [{ source = 0, volume = 0.5, muted = true }] }  # Own levels (format 17, ADR-0091)
//! # transition_in = { kind = "fade", duration_ms = 800 }  # either part optional (format 20, ADR-0135)
//!
//! [[scenes.items]]
//! id = 0
//! source = 0                   # or: scene = <id> for a nested scene
//! layout = { position = [0.0, 0.0], size = [1920.0, 1080.0] }
//! # show_for_ms = 5000         # timed show while hidden (format 13, ADR-0105)
//! ```
//!
//! Layouts may also have `rotation` (degrees), `crop` (`[left, top, right, bottom]`
//! source pixels), `opacity` and `style` (ADR-0053, format 2: `shape = "rectangle" |
//! "circle" | "ellipse"`, `radius` for rectangles, `border = { width, color =
//! "#rrggbbaa" }`, `shadow = { offset = [x, y], blur, color }`) and `scroll` (format 23,
//! ADR-0150: `{ speed, direction?, gap?, repeat?, start?, fade?, ramp_ms? }`); items `visible = false`,
//! `locked = true` (format 3), `show_for_ms` (1 to 600000, format 13, ADR-0105; omitted when unset) and `overrides = [{ canvas = 1, layout = {...} }]`; sources `filters = [{ id, type,
//! enabled, settings }]`, `volume` (0 to 1, default 1) and `muted = true` (format 3,
//! ADR-0059), `sync_offset_ms` (-200 to 200, default 0; format 4, ADR-0062), `hints = { target_app = "...", target_title = "..." }` (format 8, ADR-0077). Setting values are TOML booleans, integers, floats or
//! strings. Loading validates the whole collection
//! ([`SceneCollection::from_parts`]).

use rbs_scene::activity::{MAX_LINGER_S, OnReturn, Run, SourceActivity};
use std::collections::BTreeMap;

use rbs_core::media::Size;
use rbs_core::timing::FrameRate;
use rbs_graph::node::NodeTypeId;
use rbs_scene::model::{
    Border, Canvas, CanvasId, CollectionGlide, CollectionTransition, Crop, DEFAULT_SCROLL_RAMP_MS,
    FadeSettings, Fallback, Filter, FilterId, GlideEasing, ItemContent, ItemId, ItemStyle, Layout,
    MasterAudio, RecordSeparately, Scene, SceneAudio, SceneId, SceneItem, SceneLevel, SceneScope,
    Scroll, ScrollDirection, ScrollRepeat, ScrollStart, SettingValue, Settings, Shadow, Shape,
    Source, SourceAudio, SourceGlide, SourceId, SourceMonitor, SourceRegion, StingerSettings,
    SwipeDirection, SwipeSettings, TransitionKind, VideoAs,
};
use rbs_scene::{ChromeFill, DisplayChrome};
use rbs_scene::{IdCounters, MAX_GLIDE_MS, SceneCollection, SceneTransition};
use serde::{Deserialize, Serialize};

use crate::document::{self, BodyError, Document};

#[derive(Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct Body {
    #[serde(default)]
    next: Next,
    /// Collection morph (format 12, ADR-0100); absent: defaults.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    glide: Option<GlideDoc>,
    /// The collection transition (format 19, ADR-0128); absent: the default (Move).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    transition: Option<TransitionDoc>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    canvases: Vec<CanvasDoc>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    sources: Vec<SourceDoc>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    scenes: Vec<SceneDoc>,
}

/// Collection or source glide in the document (ADR-0100).
#[derive(Serialize, Deserialize, Clone, Copy)]
#[serde(deny_unknown_fields)]
struct GlideDoc {
    #[serde(
        default = "default_glide_ms",
        skip_serializing_if = "is_default_glide_ms"
    )]
    duration_ms: u32,
    #[serde(default, skip_serializing_if = "GlideEasingDoc::is_ease_in_out")]
    easing: GlideEasingDoc,
}

fn default_glide_ms() -> u32 {
    400
}

fn is_default_glide_ms(v: &u32) -> bool {
    *v == 400
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum GlideEasingDoc {
    Linear,
    #[default]
    EaseInOut,
}

impl GlideEasingDoc {
    fn is_ease_in_out(&self) -> bool {
        *self == Self::EaseInOut
    }
}

impl From<GlideEasing> for GlideEasingDoc {
    fn from(e: GlideEasing) -> Self {
        match e {
            GlideEasing::Linear => Self::Linear,
            GlideEasing::EaseInOut => Self::EaseInOut,
            _ => Self::EaseInOut,
        }
    }
}

impl From<GlideEasingDoc> for GlideEasing {
    fn from(e: GlideEasingDoc) -> Self {
        match e {
            GlideEasingDoc::Linear => Self::Linear,
            GlideEasingDoc::EaseInOut => Self::EaseInOut,
        }
    }
}

impl From<CollectionGlide> for GlideDoc {
    fn from(g: CollectionGlide) -> Self {
        Self {
            duration_ms: g.duration_ms,
            easing: g.easing.into(),
        }
    }
}

impl From<GlideDoc> for CollectionGlide {
    fn from(g: GlideDoc) -> Self {
        Self {
            duration_ms: g.duration_ms,
            easing: g.easing.into(),
        }
    }
}

impl From<SourceGlide> for GlideDoc {
    fn from(g: SourceGlide) -> Self {
        Self {
            duration_ms: g.duration_ms,
            easing: g.easing.into(),
        }
    }
}

impl From<GlideDoc> for SourceGlide {
    fn from(g: GlideDoc) -> Self {
        Self {
            duration_ms: g.duration_ms,
            easing: g.easing.into(),
        }
    }
}

/// The collection transition in the document (ADR-0128). Each part is omitted while
/// it holds its default.
#[derive(Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct TransitionDoc {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    kind: Option<String>,
    /// The kind outside studio mode (format 27, ADR-0153); absent: Move.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    direct: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    fade: Option<FadeDoc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    swipe: Option<SwipeDoc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    slide: Option<SwipeDoc>,
    /// Stinger's settings (format 21, ADR-0140).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    stinger: Option<StingerDoc>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StingerDoc {
    /// The media-file source's id; absent: none chosen.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    source: Option<u32>,
    /// The user's cut point; absent: the found point.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cut_ms: Option<u32>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FadeDoc {
    duration_ms: u32,
    easing: GlideEasingDoc,
    /// `"#rrggbb"`; absent: a crossfade.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    via: Option<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SwipeDoc {
    duration_ms: u32,
    easing: GlideEasingDoc,
    direction: String,
}

fn rgb_text(c: [u8; 3]) -> String {
    format!("#{:02x}{:02x}{:02x}", c[0], c[1], c[2])
}

fn rgb_parse(t: &str) -> Result<[u8; 3], BodyError> {
    let hex = t
        .strip_prefix('#')
        .filter(|h| h.len() == 6 && h.is_ascii())
        .ok_or_else(|| BodyError::Value(format!("`{t}` is not a colour of the form #rrggbb")))?;
    let byte = |i: usize| {
        u8::from_str_radix(&hex[i..i + 2], 16)
            .map_err(|_| BodyError::Value(format!("`{t}` is not a colour of the form #rrggbb")))
    };
    Ok([byte(0)?, byte(2)?, byte(4)?])
}

impl From<CollectionTransition> for TransitionDoc {
    fn from(t: CollectionTransition) -> Self {
        let d = CollectionTransition::default();
        let swipe = |s: SwipeSettings| SwipeDoc {
            duration_ms: s.duration_ms,
            easing: s.easing.into(),
            direction: s.direction.as_str().to_owned(),
        };
        Self {
            kind: (t.kind != d.kind).then(|| t.kind.as_str().to_owned()),
            direct: (t.direct != d.direct).then(|| t.direct.as_str().to_owned()),
            fade: (t.fade != d.fade).then(|| FadeDoc {
                duration_ms: t.fade.duration_ms,
                easing: t.fade.easing.into(),
                via: t.fade.via.map(rgb_text),
            }),
            swipe: (t.swipe != d.swipe).then(|| swipe(t.swipe)),
            slide: (t.slide != d.slide).then(|| swipe(t.slide)),
            stinger: (t.stinger != d.stinger).then(|| StingerDoc {
                source: t.stinger.source.map(SourceId::get),
                cut_ms: t.stinger.cut_ms,
            }),
        }
    }
}

impl TryFrom<TransitionDoc> for CollectionTransition {
    type Error = BodyError;

    fn try_from(doc: TransitionDoc) -> Result<Self, BodyError> {
        let mut t = CollectionTransition::default();
        if let Some(kind) = doc.kind {
            t.kind = TransitionKind::parse(&kind)
                .ok_or_else(|| BodyError::Value(format!("unknown transition kind `{kind}`")))?;
        }
        if let Some(kind) = doc.direct {
            t.direct = TransitionKind::parse(&kind)
                .ok_or_else(|| BodyError::Value(format!("unknown transition kind `{kind}`")))?;
        }
        if let Some(f) = doc.fade {
            t.fade = FadeSettings {
                duration_ms: f.duration_ms,
                easing: f.easing.into(),
                via: f.via.as_deref().map(rgb_parse).transpose()?,
            };
        }
        let swipe = |s: SwipeDoc| -> Result<SwipeSettings, BodyError> {
            Ok(SwipeSettings {
                duration_ms: s.duration_ms,
                easing: s.easing.into(),
                direction: SwipeDirection::parse(&s.direction).ok_or_else(|| {
                    BodyError::Value(format!("unknown direction `{}`", s.direction))
                })?,
            })
        };
        if let Some(s) = doc.swipe {
            t.swipe = swipe(s)?;
        }
        if let Some(s) = doc.slide {
            t.slide = swipe(s)?;
        }
        if let Some(s) = doc.stinger {
            t.stinger = StingerSettings {
                source: s.source.map(SourceId::from_raw),
                cut_ms: s.cut_ms,
            };
        }
        Ok(t)
    }
}

#[derive(Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct Next {
    #[serde(default)]
    canvas: u32,
    #[serde(default)]
    source: u32,
    #[serde(default)]
    scene: u32,
    #[serde(default)]
    item: u32,
    #[serde(default)]
    filter: u32,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CanvasDoc {
    id: u32,
    name: String,
    width: u32,
    height: u32,
    frame_rate: [u32; 2],
    #[serde(default, skip_serializing_if = "Option::is_none")]
    program: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    preview: Option<u32>,
    /// The canvas's global scene (format 5, ADR-0065).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    globals: Option<u32>,
    /// The canvas's master volume and mute, written only when not the default
    /// (format 7, ADR-0081).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    master: Option<MasterDoc>,
    /// The canvas whose program scene this one shows (format 9, ADR-0093).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    follow: Option<u32>,
    /// What a follower shows when it may not show the followed scene (format 9).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    fallback: Option<FallbackDoc>,
    /// Scene list order (format 11). Absent: id order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    order: Vec<u32>,
    /// Layout space width (format 15, ADR-0110). Absent: `width`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    layout_width: Option<u32>,
    /// Layout space height (format 15, ADR-0110). Absent: `height`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    layout_height: Option<u32>,
}

/// A follower's fallback: `"keep_last"` or `{ scene = id }`.
#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum FallbackDoc {
    Keep(KeepLast),
    Scene { scene: u32 },
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum KeepLast {
    KeepLast,
}

/// A canvas's master: each key is optional and defaults to full volume, not muted.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct MasterDoc {
    #[serde(default = "one", skip_serializing_if = "is_one")]
    volume: f64,
    #[serde(default, skip_serializing_if = "is_false")]
    muted: bool,
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(untagged)]
enum Value {
    Bool(bool),
    Int(i64),
    Float(f64),
    Text(String),
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SourceDoc {
    id: u32,
    name: String,
    #[serde(rename = "type")]
    type_id: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    settings: BTreeMap<String, Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    filters: Vec<FilterDoc>,
    #[serde(default = "one", skip_serializing_if = "is_one")]
    volume: f64,
    #[serde(default, skip_serializing_if = "is_false")]
    muted: bool,
    #[serde(default, skip_serializing_if = "is_zero_ms")]
    sync_offset_ms: i32,
    #[serde(default, skip_serializing_if = "is_false")]
    mixer_hidden: bool,
    /// Heard on the monitor output (format 10, ADR-0097); absent: off.
    #[serde(default, skip_serializing_if = "MonitorDoc::is_off")]
    monitor: MonitorDoc,
    /// Slide override (format 12, ADR-0100); absent: collection glide.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    glide: Option<GlideDoc>,
    /// Hints for finding the target again (`target_app`, `target_title`; format 8, ADR-0077).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    hints: BTreeMap<String, String>,
    /// Full-picture insets `[left, top, right, bottom]` (format 14, ADR-0103). Absent: whole picture.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    region: Option<[u32; 4]>,
    /// Display chrome (format 14, ADR-0104). Absent: off. Only a display source may have it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    chrome: Option<ChromeDoc>,
    /// Separate files for this source (format 16, ADR-0086). Absent: neither feed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    record_separately: Option<RecordSeparatelyDoc>,
    /// When the source runs and what it does when it returns (format 24, ADR-0152).
    /// Absent: the type's default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    activity: Option<ActivityDoc>,
}

/// A source's activity policy (ADR-0152): every key optional, absent is the type's
/// default. Values the type does not allow load and are reported by the binder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ActivityDoc {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    run: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    on_return: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    linger_s: Option<u16>,
}

impl ActivityDoc {
    fn from(a: SourceActivity) -> Option<Self> {
        (!a.is_default()).then(|| Self {
            run: a.run.map(|r| r.as_str().to_owned()),
            on_return: a.on_return.map(|o| o.as_str().to_owned()),
            linger_s: a.linger_s,
        })
    }

    fn activity(&self, source: &str) -> Result<SourceActivity, BodyError> {
        let run = self
            .run
            .as_deref()
            .map(|r| {
                Run::parse(r).ok_or_else(|| {
                    BodyError::Value(format!("{source}: unknown `activity.run` {r:?}"))
                })
            })
            .transpose()?;
        let on_return = self
            .on_return
            .as_deref()
            .map(|o| {
                OnReturn::parse(o).ok_or_else(|| {
                    BodyError::Value(format!("{source}: unknown `activity.on_return` {o:?}"))
                })
            })
            .transpose()?;
        if self.linger_s.is_some_and(|l| l > MAX_LINGER_S) {
            return Err(BodyError::Value(format!(
                "{source}: `activity.linger_s` is over {MAX_LINGER_S}"
            )));
        }
        Ok(SourceActivity::new(run, on_return, self.linger_s))
    }
}

/// Which feeds of a source are recorded as their own files (ADR-0086).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RecordSeparatelyDoc {
    #[serde(default, skip_serializing_if = "is_false")]
    video: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    audio: bool,
    /// The picture's form (format 18, ADR-0117): `"image"` or `"video"`. Absent:
    /// automatic. Written only with `video`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    video_as: Option<String>,
}

impl RecordSeparatelyDoc {
    fn from(flag: RecordSeparately) -> Self {
        Self {
            video: flag.video,
            audio: flag.audio,
            video_as: (flag.video && flag.video_as != VideoAs::Auto)
                .then(|| flag.video_as.as_str().to_owned()),
        }
    }

    fn flag(&self) -> Result<RecordSeparately, BodyError> {
        let video_as = match self.video_as.as_deref() {
            None => VideoAs::Auto,
            Some(text) => VideoAs::parse(text)
                .ok_or_else(|| BodyError::Value(format!("unknown `video_as` value {text:?}")))?,
        };
        if video_as != VideoAs::Auto && !self.video {
            return Err(BodyError::Value(
                "`video_as` needs `video = true`".to_owned(),
            ));
        }
        Ok(RecordSeparately::new(self.video, self.audio).with_video_as(video_as))
    }
}

/// Display chrome in the document (ADR-0104).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ChromeDoc {
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    top: u32,
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    right: u32,
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    bottom: u32,
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    left: u32,
    /// `"cover"` (default) or `"fit"`.
    #[serde(default, skip_serializing_if = "FillName::is_cover")]
    fill: FillName,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum FillName {
    #[default]
    Cover,
    Fit,
}

impl FillName {
    fn is_cover(&self) -> bool {
        *self == Self::Cover
    }
}

impl ChromeDoc {
    fn from(chrome: DisplayChrome) -> Self {
        Self {
            top: chrome.top,
            right: chrome.right,
            bottom: chrome.bottom,
            left: chrome.left,
            fill: match chrome.fill {
                ChromeFill::Cover => FillName::Cover,
                ChromeFill::Fit => FillName::Fit,
                _ => FillName::Cover,
            },
        }
    }

    fn chrome(&self) -> Result<DisplayChrome, BodyError> {
        let fill = match self.fill {
            FillName::Cover => ChromeFill::Cover,
            FillName::Fit => ChromeFill::Fit,
        };
        Ok(DisplayChrome::new(self.top, self.right, self.bottom, self.left).with_fill(fill))
    }
}

fn is_zero_u32(v: &u32) -> bool {
    *v == 0
}

/// A source's monitor mode in the document (ADR-0097 decision 5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum MonitorDoc {
    #[default]
    Off,
    Only,
    Also,
}

impl MonitorDoc {
    fn is_off(&self) -> bool {
        *self == Self::Off
    }
}

impl From<SourceMonitor> for MonitorDoc {
    fn from(m: SourceMonitor) -> Self {
        match m {
            SourceMonitor::Off => Self::Off,
            SourceMonitor::Only => Self::Only,
            SourceMonitor::Also => Self::Also,
        }
    }
}

impl From<MonitorDoc> for SourceMonitor {
    fn from(m: MonitorDoc) -> Self {
        match m {
            MonitorDoc::Off => Self::Off,
            MonitorDoc::Only => Self::Only,
            MonitorDoc::Also => Self::Also,
        }
    }
}

fn is_zero_ms(v: &i32) -> bool {
    *v == 0
}

fn is_false(v: &bool) -> bool {
    !*v
}

fn yes() -> bool {
    true
}

fn is_true(v: &bool) -> bool {
    *v
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FilterDoc {
    id: u32,
    #[serde(rename = "type")]
    type_id: String,
    #[serde(default = "yes", skip_serializing_if = "is_true")]
    enabled: bool,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    settings: BTreeMap<String, Value>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SceneDoc {
    id: u32,
    name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    canvas: Option<u32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    items: Vec<ItemDoc>,
    /// The scene's own levels (format 17, ADR-0091). Absent: Shared levels; an empty
    /// table: Own levels with no entries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    audio: Option<SceneAudioDoc>,
    /// The transition into this scene (format 20, ADR-0135). Absent: the collection's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    transition_in: Option<SceneTransitionDoc>,
}

/// A scene's transition override in the document (ADR-0135 decision 9).
#[derive(Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct SceneTransitionDoc {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    duration_ms: Option<u32>,
}

impl SceneTransitionDoc {
    fn of(t: SceneTransition) -> Self {
        Self {
            kind: t.kind.map(|k| k.as_str().to_owned()),
            duration_ms: t.duration_ms,
        }
    }

    fn transition(self, scene: &str) -> Result<Option<SceneTransition>, BodyError> {
        let kind = self
            .kind
            .map(|k| {
                TransitionKind::parse(&k).ok_or_else(|| {
                    BodyError::Value(format!("scene {scene:?}: unknown transition kind `{k}`"))
                })
            })
            .transpose()?;
        let t = SceneTransition {
            kind,
            duration_ms: self.duration_ms,
        };
        if !t.is_valid() {
            return Err(BodyError::Value(format!(
                "scene {scene:?}: transition duration above {MAX_GLIDE_MS} ms"
            )));
        }
        Ok(Some(t).filter(|t| !t.is_empty()))
    }
}

/// A scene's own levels in the document (ADR-0091).
#[derive(Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct SceneAudioDoc {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    levels: Vec<SceneLevelDoc>,
}

/// One entry of a scene's own levels: a source and the fields the scene sets.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SceneLevelDoc {
    source: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    volume: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    muted: Option<bool>,
}

impl SceneAudioDoc {
    fn of(audio: &SceneAudio) -> Self {
        Self {
            levels: audio
                .levels
                .iter()
                .map(|(id, l)| SceneLevelDoc {
                    source: id.get(),
                    volume: l.volume.map(f64::from),
                    muted: l.muted,
                })
                .collect(),
        }
    }

    fn audio(self, scene: &str) -> Result<SceneAudio, BodyError> {
        let mut levels = BTreeMap::new();
        for l in self.levels {
            // Range checks are the collection's (`InvalidVolume`).
            #[allow(clippy::cast_possible_truncation)] // A volume is 0 to 1.
            let level = SceneLevel::new(l.volume.map(|v| v as f32), l.muted);
            if levels.insert(SourceId::from_raw(l.source), level).is_some() {
                return Err(BodyError::Value(format!(
                    "scene {scene:?} lists source {} twice in its levels",
                    l.source
                )));
            }
        }
        Ok(SceneAudio::new(levels))
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ItemDoc {
    id: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    source: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    scene: Option<u32>,
    #[serde(default = "yes", skip_serializing_if = "is_true")]
    visible: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    locked: bool,
    /// Timed-show duration while hidden (format 13, ADR-0105). Absent: none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    show_for_ms: Option<u32>,
    layout: LayoutDoc,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    overrides: Vec<OverrideDoc>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct OverrideDoc {
    canvas: u32,
    layout: LayoutDoc,
}

fn one() -> f64 {
    1.0
}

fn is_zero(v: &f64) -> bool {
    *v == 0.0
}

fn is_one(v: &f64) -> bool {
    *v == 1.0
}

fn no_crop(v: &[u32; 4]) -> bool {
    *v == [0; 4]
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LayoutDoc {
    position: [f64; 2],
    size: [f64; 2],
    #[serde(default, skip_serializing_if = "is_zero")]
    rotation: f64,
    #[serde(default, skip_serializing_if = "no_crop")]
    crop: [u32; 4],
    #[serde(default = "one", skip_serializing_if = "is_one")]
    opacity: f64,
    #[serde(default, skip_serializing_if = "StyleDoc::is_plain")]
    style: StyleDoc,
    /// Format 23 (ADR-0150).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    scroll: Option<ScrollDoc>,
}

#[derive(Serialize, Deserialize, Default, Clone, Copy, PartialEq)]
#[serde(rename_all = "lowercase")]
enum DirectionName {
    #[default]
    Left,
    Right,
    Up,
    Down,
}

#[derive(Serialize, Deserialize, Default, Clone, Copy, PartialEq)]
#[serde(rename_all = "lowercase")]
enum RepeatName {
    #[default]
    Loop,
    Once,
}

#[derive(Serialize, Deserialize, Default, Clone, Copy, PartialEq)]
#[serde(rename_all = "lowercase")]
enum StartName {
    #[default]
    Inside,
    Outside,
}

/// A scroll's `on_return` (format 28, ADR-0152 decision 6).
#[derive(Serialize, Deserialize, Default, Clone, Copy, PartialEq)]
#[serde(rename_all = "lowercase")]
enum ScrollReturnName {
    #[default]
    Restart,
    Continue,
}

fn is_default<T: Default + PartialEq>(v: &T) -> bool {
    *v == T::default()
}

fn default_ramp_ms() -> u32 {
    DEFAULT_SCROLL_RAMP_MS
}

fn is_default_ramp_ms(v: &u32) -> bool {
    *v == DEFAULT_SCROLL_RAMP_MS
}

/// A layout's `scroll` (format 23, ADR-0150). Ranges are checked with the collection.
#[derive(Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct ScrollDoc {
    #[serde(default, skip_serializing_if = "is_default")]
    direction: DirectionName,
    speed: f64,
    #[serde(default, skip_serializing_if = "is_zero")]
    gap: f64,
    #[serde(default, skip_serializing_if = "is_default")]
    repeat: RepeatName,
    #[serde(default, skip_serializing_if = "is_default")]
    start: StartName,
    #[serde(default, skip_serializing_if = "is_zero")]
    fade: f64,
    #[serde(
        default = "default_ramp_ms",
        skip_serializing_if = "is_default_ramp_ms"
    )]
    ramp_ms: u32,
    #[serde(default, skip_serializing_if = "is_default")]
    on_return: ScrollReturnName,
}

impl ScrollDoc {
    fn of(s: &Scroll) -> Self {
        Self {
            direction: match s.direction {
                ScrollDirection::Right => DirectionName::Right,
                ScrollDirection::Up => DirectionName::Up,
                ScrollDirection::Down => DirectionName::Down,
                // Left, and a direction added by a newer build.
                _ => DirectionName::Left,
            },
            speed: wide(s.speed),
            gap: wide(s.gap),
            repeat: match s.repeat {
                ScrollRepeat::Once => RepeatName::Once,
                _ => RepeatName::Loop,
            },
            start: match s.start {
                ScrollStart::Outside => StartName::Outside,
                _ => StartName::Inside,
            },
            fade: wide(s.fade),
            ramp_ms: s.ramp_ms,
            on_return: match s.on_return {
                OnReturn::Continue => ScrollReturnName::Continue,
                _ => ScrollReturnName::Restart,
            },
        }
    }

    fn scroll(&self) -> Scroll {
        let direction = match self.direction {
            DirectionName::Left => ScrollDirection::Left,
            DirectionName::Right => ScrollDirection::Right,
            DirectionName::Up => ScrollDirection::Up,
            DirectionName::Down => ScrollDirection::Down,
        };
        Scroll::new(direction, self.speed as f32)
            .with_gap(self.gap as f32)
            .with_repeat(match self.repeat {
                RepeatName::Loop => ScrollRepeat::Loop,
                RepeatName::Once => ScrollRepeat::Once,
            })
            .with_start(match self.start {
                StartName::Inside => ScrollStart::Inside,
                StartName::Outside => ScrollStart::Outside,
            })
            .with_fade(self.fade as f32)
            .with_ramp_ms(self.ramp_ms)
            .with_on_return(match self.on_return {
                ScrollReturnName::Restart => OnReturn::Restart,
                ScrollReturnName::Continue => OnReturn::Continue,
            })
    }
}

#[derive(Serialize, Deserialize, Default, Clone, Copy, PartialEq)]
#[serde(rename_all = "lowercase")]
enum ShapeName {
    #[default]
    Rectangle,
    Circle,
    Ellipse,
    Triangle,
    Arrow,
}

fn is_rectangle(v: &ShapeName) -> bool {
    *v == ShapeName::Rectangle
}

#[derive(Serialize, Deserialize, Default, PartialEq)]
#[serde(deny_unknown_fields)]
struct StyleDoc {
    #[serde(default, skip_serializing_if = "is_rectangle")]
    shape: ShapeName,
    #[serde(default, skip_serializing_if = "is_zero")]
    radius: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    border: Option<BorderDoc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    shadow: Option<ShadowDoc>,
}

#[derive(Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct BorderDoc {
    width: f64,
    color: String,
}

#[derive(Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct ShadowDoc {
    #[serde(default)]
    offset: [f64; 2],
    #[serde(default)]
    blur: f64,
    color: String,
}

/// `#rrggbbaa`, the form documents write.
fn color_doc(c: [u8; 4]) -> String {
    format!("#{:02x}{:02x}{:02x}{:02x}", c[0], c[1], c[2], c[3])
}

/// `#rrggbb` (opaque) or `#rrggbbaa`, hexadecimal digits in either case.
fn color(text: &str) -> Result<[u8; 4], BodyError> {
    let bad = || BodyError::Value(format!("colour {text:?} is not #rrggbb or #rrggbbaa"));
    let hex = text.strip_prefix('#').ok_or_else(bad)?;
    if !(hex.len() == 6 || hex.len() == 8) || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(bad());
    }
    let byte = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).map_err(|_| bad());
    let alpha = if hex.len() == 8 { byte(6)? } else { 255 };
    Ok([byte(0)?, byte(2)?, byte(4)?, alpha])
}

impl StyleDoc {
    fn is_plain(&self) -> bool {
        *self == Self::default()
    }

    fn of(s: &ItemStyle) -> Self {
        let (shape, radius) = match s.shape {
            Shape::Rectangle { radius } => (ShapeName::Rectangle, wide(radius)),
            Shape::Circle => (ShapeName::Circle, 0.0),
            Shape::Ellipse => (ShapeName::Ellipse, 0.0),
            Shape::Triangle => (ShapeName::Triangle, 0.0),
            Shape::Arrow => (ShapeName::Arrow, 0.0),
            // A shape added by a newer build is stored as a plain rectangle.
            _ => (ShapeName::Rectangle, 0.0),
        };
        Self {
            shape,
            radius,
            border: s.border.map(|b| BorderDoc {
                width: wide(b.width),
                color: color_doc(b.color),
            }),
            shadow: s.shadow.map(|sh| ShadowDoc {
                offset: sh.offset.map(wide),
                blur: wide(sh.blur),
                color: color_doc(sh.color),
            }),
        }
    }

    /// The model's style; validity of the numbers is checked with the collection.
    fn style(&self) -> Result<ItemStyle, BodyError> {
        let shape = match self.shape {
            ShapeName::Rectangle => Shape::Rectangle {
                radius: self.radius as f32,
            },
            _ if self.radius != 0.0 => {
                return Err(BodyError::Value(
                    "radius is only for shape = \"rectangle\"".into(),
                ));
            }
            ShapeName::Circle => Shape::Circle,
            ShapeName::Ellipse => Shape::Ellipse,
            ShapeName::Triangle => Shape::Triangle,
            ShapeName::Arrow => Shape::Arrow,
        };
        let border = match &self.border {
            Some(b) => Some(Border::new(b.width as f32, color(&b.color)?)),
            None => None,
        };
        let shadow = match &self.shadow {
            Some(sh) => Some(Shadow::new(
                sh.offset.map(|v| v as f32),
                sh.blur as f32,
                color(&sh.color)?,
            )),
            None => None,
        };
        Ok(ItemStyle::new(shape)
            .with_border(border)
            .with_shadow(shadow))
    }
}

/// An `f32` as the shortest `f64` that reads back as the same `f32` (so `0.1` is
/// written as `0.1`, not `0.10000000149011612`).
fn wide(v: f32) -> f64 {
    format!("{v}").parse().unwrap_or(f64::from(v))
}

impl LayoutDoc {
    fn of(l: &Layout) -> Self {
        let c = l.crop;
        Self {
            position: l.position.map(wide),
            size: l.size.map(wide),
            rotation: wide(l.rotation),
            crop: [c.left, c.top, c.right, c.bottom],
            opacity: wide(l.opacity),
            style: StyleDoc::of(&l.style),
            scroll: l.scroll.as_ref().map(ScrollDoc::of),
        }
    }

    fn layout(&self) -> Result<Layout, BodyError> {
        let [left, top, right, bottom] = self.crop;
        Ok(
            Layout::new(self.position.map(|v| v as f32), self.size.map(|v| v as f32))
                .with_rotation(self.rotation as f32)
                .with_crop(Crop::new(left, top, right, bottom))
                .with_opacity(self.opacity as f32)
                .with_style(self.style.style()?)
                .with_scroll(self.scroll.as_ref().map(ScrollDoc::scroll)),
        )
    }
}

fn settings_doc(s: &Settings) -> Result<BTreeMap<String, Value>, BodyError> {
    s.iter()
        .map(|(k, v)| {
            let v = match v {
                SettingValue::Bool(b) => Value::Bool(*b),
                SettingValue::Int(i) => Value::Int(*i),
                SettingValue::Float(f) => Value::Float(*f),
                SettingValue::Text(t) => Value::Text(t.clone()),
                _ => {
                    return Err(BodyError::Value(format!(
                        "setting {k:?} has a type documents cannot hold"
                    )));
                }
            };
            Ok((k.clone(), v))
        })
        .collect()
}

/// The browser source type and the setting keys format 22 adds to it. The type and
/// the keys are `rbs-sources` names, which this crate does not depend on.
const BROWSER_SOURCE: &str = "org.rbs.source.browser";
const MEDIA_SOURCE: &str = "org.rbs.source.media";

/// Moves a browser source's `keep_running` setting into its activity policy (format
/// 26, ADR-0152 decision 10): true is `run = "placed"`, false is the type's default,
/// and the key is dropped. A saved `run` wins over the old key.
fn migrate_keep_running(body: &mut toml::Table) {
    let sources = body
        .get_mut("sources")
        .and_then(toml::Value::as_array_mut)
        .into_iter()
        .flatten()
        .filter_map(toml::Value::as_table_mut)
        .filter(|s| s.get("type").and_then(toml::Value::as_str) == Some(BROWSER_SOURCE));
    for source in sources {
        let Some(keep) = source
            .get_mut("settings")
            .and_then(toml::Value::as_table_mut)
            .and_then(|t| t.remove("keep_running"))
        else {
            continue;
        };
        let Some(keep) = keep.as_bool() else {
            // Not a flag: put it back so the source reports the bad setting.
            if let Some(t) = source
                .get_mut("settings")
                .and_then(toml::Value::as_table_mut)
            {
                t.insert("keep_running".into(), keep);
            }
            continue;
        };
        if !keep {
            continue;
        }
        let activity = source
            .entry("activity")
            .or_insert_with(|| toml::Value::Table(toml::Table::new()));
        if let Some(activity) = activity.as_table_mut() {
            activity
                .entry("run")
                .or_insert_with(|| toml::Value::String("placed".into()));
        }
    }
}

/// Moves a media source's `restart_when_shown` setting into its activity policy
/// (format 25, ADR-0152 decision 10): true is `on_return = "restart"`, false is
/// `"resume"`, and the key is dropped. A saved `on_return` wins over the old key.
fn migrate_restart_when_shown(body: &mut toml::Table) {
    let sources = body
        .get_mut("sources")
        .and_then(toml::Value::as_array_mut)
        .into_iter()
        .flatten()
        .filter_map(toml::Value::as_table_mut)
        .filter(|s| s.get("type").and_then(toml::Value::as_str) == Some(MEDIA_SOURCE));
    for source in sources {
        let Some(restart) = source
            .get_mut("settings")
            .and_then(toml::Value::as_table_mut)
            .and_then(|t| t.remove("restart_when_shown"))
        else {
            continue;
        };
        let Some(restart) = restart.as_bool() else {
            // Not a flag: put it back so the source reports the bad setting.
            if let Some(t) = source
                .get_mut("settings")
                .and_then(toml::Value::as_table_mut)
            {
                t.insert("restart_when_shown".into(), restart);
            }
            continue;
        };
        let activity = source
            .entry("activity")
            .or_insert_with(|| toml::Value::Table(toml::Table::new()));
        if let Some(activity) = activity.as_table_mut() {
            let on_return = if restart { "restart" } else { "resume" };
            activity
                .entry("on_return")
                .or_insert_with(|| toml::Value::String(on_return.into()));
        }
    }
}
const BROWSER_PAGE_KEYS: [&str; 11] = [
    "css",
    "reload_on_file_change",
    "keep_signed_in",
    "profile_id",
    "reload_every_s",
    "script",
    "script_on_load",
    "script_every_s",
    "zoom_percent",
    "keep_running",
    "retry_on_error",
];

/// True for a browser source that has a format 22 page setting.
fn browser_page_settings(s: &Source) -> bool {
    s.type_id.as_str() == BROWSER_SOURCE
        && BROWSER_PAGE_KEYS
            .iter()
            .any(|k| s.settings.contains_key(*k))
}

fn settings(s: BTreeMap<String, Value>) -> Settings {
    s.into_iter()
        .map(|(k, v)| {
            let v = match v {
                Value::Bool(b) => SettingValue::Bool(b),
                Value::Int(i) => SettingValue::Int(i),
                Value::Float(f) => SettingValue::Float(f),
                Value::Text(t) => SettingValue::Text(t),
            };
            (k, v)
        })
        .collect()
}

/// True when an item layout or a per-canvas override in `body` has a `scroll` key.
fn any_layout_scrolls(body: &toml::Table) -> bool {
    let has = |v: &toml::Value| {
        v.get("layout")
            .and_then(toml::Value::as_table)
            .is_some_and(|l| l.contains_key("scroll"))
    };
    body.get("scenes")
        .and_then(toml::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|s| s.get("items").and_then(toml::Value::as_array))
        .flatten()
        .any(|item| {
            has(item)
                || item
                    .get("overrides")
                    .and_then(toml::Value::as_array)
                    .is_some_and(|o| o.iter().any(has))
        })
}

/// An older file that uses keys only format `since` has.
fn later(found: u32, keys: &'static str, since: u32) -> BodyError {
    BodyError::LaterKey { found, keys, since }
}

impl Document for SceneCollection {
    const KIND: &'static str = "scene-collection";
    const FORMAT: u32 = 28;

    fn write_format(&self) -> u32 {
        // An older build would start a continuing scroll over (ADR-0152 decision 6).
        let continues = |l: &Layout| l.scroll.is_some_and(|s| s.on_return == OnReturn::Continue);
        if self.scenes().any(|s| {
            s.items
                .iter()
                .any(|i| continues(&i.layout) || i.overrides.values().any(continues))
        }) {
            return 28;
        }
        // An older build would run every change with the rail's kind (ADR-0153).
        if self.transition().direct != CollectionTransition::default().direct {
            return 27;
        }
        // A format 25 build does not run a page while it is hidden and has no
        // `keep_running` key to read, so a background page is written as 26.
        if self.sources().any(|s| {
            s.type_id.as_str() == BROWSER_SOURCE
                && s.activity.run == Some(rbs_scene::activity::Run::Placed)
        }) {
            return 26;
        }
        // A format 24 build reads `on_return` but still plays a film by its old
        // `restart_when_shown` key, so a file that sets one is written as 25.
        if self.sources().any(|s| s.activity.on_return.is_some()) {
            return 25;
        }
        if self.sources().any(|s| !s.activity.is_default()) {
            return 24;
        }
        let scrolls = self.scenes().any(|s| {
            s.items.iter().any(|i| {
                i.layout.scroll.is_some() || i.overrides.values().any(|l| l.scroll.is_some())
            })
        });
        if scrolls {
            return 23;
        }
        let t = self.transition();
        let stinger = t.kind == TransitionKind::Stinger
            || t.stinger != StingerSettings::default()
            || self
                .scenes()
                .any(|s| s.transition_in.and_then(|t| t.kind) == Some(TransitionKind::Stinger));
        // Format 18 and older read a 0 ms glide as Cut, so such a collection is written
        // as 19 to keep its own transition (ADR-0128 decision 3).
        if self.sources().any(browser_page_settings) {
            22
        } else if stinger {
            21
        } else if self.scenes().any(|s| s.transition_in.is_some()) {
            20
        } else if !self.transition().is_default() || self.glide().duration_ms == 0 {
            19
        } else if self
            .sources()
            .any(|s| s.record_separately.video && s.record_separately.video_as != VideoAs::Auto)
        {
            18
        } else if self.scenes().any(Scene::own_levels) {
            17
        } else if self.sources().any(|s| !s.record_separately.is_none()) {
            16
        } else {
            15
        }
    }

    /// Format 2 adds the optional layout `style` (ADR-0053); a format 1 body is a
    /// format 2 body without styles. Format 3 adds the optional item `locked` and
    /// source `volume` and `muted` (ADR-0059); a format 2 body is a format 3 body
    /// without them. Format 4 adds the optional source `sync_offset_ms` (ADR-0062); a
    /// format 3 body is a format 4 body without it. Format 5 adds the optional canvas
    /// `globals` (ADR-0065); a format 4 body is a format 5 body without it. Format 6 adds the optional source
    /// `mixer_hidden` (ADR-0080); a format 5 body is a format 6 body without it. Format 7 adds the optional canvas
    /// `master` (ADR-0081); a format 6 body is a format 7 body without it. Format 8 adds the optional source
    /// `hints` (ADR-0077); a format 7 body is a format 8 body without them. Format 9 adds the optional canvas
    /// `follow` and `fallback` (ADR-0093); a format 8 body is a format 9 body without them.
    /// Format 10 adds the optional source `monitor` (ADR-0097); a format 9 body is a
    /// format 10 body without it. Format 11 adds the optional canvas `order` (the
    /// scene list); a format 10 body is a format 11 body without it. Format 12 adds the
    /// collection `glide` and optional per-source `glide` (ADR-0100); a format 11 body
    /// is a format 12 body without them. Format 13 adds the optional item
    /// `show_for_ms` (ADR-0105); a format 12 body is a format 13 body without it.
    /// Format 14 adds the optional source `region` and `chrome` (ADR-0103, ADR-0104);
    /// a format 13 body is a format 14 body without them. Format 15 adds the optional
    /// canvas `layout_width` and `layout_height` (ADR-0110); a format 14 body is a
    /// format 15 body without them, and the layout space is then the canvas size.
    /// Format 16 adds the optional source `record_separately` (ADR-0086); a format 15
    /// body is a format 16 body without it. A file is written as 16 only when a
    /// source has the flag ([`Document::write_format`]). Format 17 adds the optional
    /// scene `audio` (ADR-0091); a format 16 body is a format 17 body without it, and a
    /// file is written as 17 only when a scene has it. Format 18 adds the optional
    /// `record_separately.video_as` (ADR-0117); a format 17 body is a format 18 body
    /// without it, and a file is written as 18 only when a source has it. Format 19
    /// adds the optional collection `transition` (ADR-0128); a format 18 or older body
    /// is a format 19 body whose transition is Move, or Cut when its glide is 0 ms, and a
    /// file is written as 19 only when it needs it. Format 20 adds the optional scene
    /// `transition_in` (ADR-0135); a format 19 or older body is a format 20 body
    /// without it, and a file is written as 20 only when a scene has it. Format 21 adds
    /// the Stinger kind and the optional `transition.stinger` (ADR-0140); a format 20 or
    /// older body is a format 21 body without them, and a file is written as 21 only
    /// when it uses them. Format 22 adds the browser page settings `css`,
    /// `reload_on_file_change`, `keep_signed_in`, `profile_id` (ADR-0132) and
    /// `reload_every_s`, `script`, `script_on_load`, `script_every_s`, `zoom_percent`,
    /// `keep_running`, `retry_on_error` (ADR-0149) of a browser source; a format 21 body
    /// is a format 22 body without them, and a file is written as 22 only when a browser
    /// source has one, so an older build refuses it instead of failing the source. Format
    /// 23 adds the optional layout `scroll` (ADR-0150); a format 22 or older body is a
    /// format 23 body without it, and a file is written as 23 only when a layout scrolls.
    /// Format 24 adds the optional source `activity` (ADR-0152); a format 23 or older
    /// body is a format 24 body without it, and a file is written as 24 only when a
    /// source has one. Format 25 moves a media source's `restart_when_shown` setting
    /// into its `activity.on_return` (true `restart`, false `resume`) and drops the
    /// key; a format 24 or older body is upgraded that way, and a file is written as
    /// 25 only when a source has an `on_return`. Format 26 moves a browser source's
    /// `keep_running` setting into its `activity.run` (true `placed`, false the type's
    /// default) and drops the key; a format 25 or older body is upgraded that way, and
    /// a file is written as 26 only when a browser source has `run = "placed"`. Format
    /// 27 adds the optional `transition.direct` (ADR-0153); a format 26 or older body is
    /// a format 27 body without it (Move outside studio mode), and a file is written as
    /// 27 only when it is not Move. Format 28 adds the optional scroll `on_return`
    /// (`continue`, ADR-0152 decision 6); a format 27 or older body is a format 28 body
    /// without it, and a file is written as 28 only when a scroll continues.
    fn upgrade(from: u32, body: &mut toml::Table) -> Result<(), BodyError> {
        let direct = body
            .get("transition")
            .and_then(toml::Value::as_table)
            .is_some_and(|t| t.contains_key("direct"));
        if (1..=26).contains(&from) && direct {
            return Err(later(from, "`transition.direct`", 27));
        }
        if (26..=27).contains(&from) {
            return Ok(());
        }
        let has_activity = body
            .get("sources")
            .and_then(toml::Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(toml::Value::as_table)
            .any(|s| s.contains_key("activity"));
        if (1..=23).contains(&from) && has_activity {
            return Err(later(from, "source `activity`", 24));
        }
        if (1..=24).contains(&from) {
            migrate_restart_when_shown(body);
        }
        if (1..=25).contains(&from) {
            migrate_keep_running(body);
        }
        if (23..=25).contains(&from) {
            return Ok(());
        }
        if (1..=22).contains(&from) && any_layout_scrolls(body) {
            return Err(later(from, "layout `scroll`", 23));
        }
        if from == 22 {
            return Ok(());
        }
        // Format 22 adds browser page settings (ADR-0132, ADR-0149) inside the open
        // `settings` table; a format 21 body is a format 22 body without them.
        if from == 21 {
            return Ok(());
        }
        let stung = body
            .get("transition")
            .and_then(toml::Value::as_table)
            .is_some_and(|t| t.contains_key("stinger"));
        if (1..=20).contains(&from) && stung {
            return Err(later(from, "`transition.stinger`", 21));
        }
        if from == 20 {
            return Ok(());
        }
        let overridden = body
            .get("scenes")
            .and_then(toml::Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(toml::Value::as_table)
            .any(|s| s.contains_key("transition_in"));
        if (1..=19).contains(&from) && overridden {
            return Err(later(from, "scene `transition_in`", 20));
        }
        match from {
            19 => Ok(()),
            1..=18 => {
                if body.contains_key("transition") {
                    return Err(later(from, "`transition`", 19));
                }
                let still = body
                    .get("glide")
                    .and_then(toml::Value::as_table)
                    .and_then(|g| g.get("duration_ms"))
                    .and_then(toml::Value::as_integer)
                    == Some(0);
                if still {
                    let mut t = toml::Table::new();
                    t.insert("kind".into(), toml::Value::String("cut".into()));
                    body.insert("transition".into(), toml::Value::Table(t));
                }
                if from == 18 {
                    return Ok(());
                }
                // Keys of a later format in an older file mean it was not written by
                // a build that knew them: refuse instead of reading them silently.
                let late = body
                    .get("canvases")
                    .and_then(toml::Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(toml::Value::as_table)
                    .any(|c| c.contains_key("follow") || c.contains_key("fallback"));
                if from < 9 && late {
                    return Err(later(from, "canvas `follow` or `fallback`", 9));
                }
                let monitored = body
                    .get("sources")
                    .and_then(toml::Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(toml::Value::as_table)
                    .any(|s| s.contains_key("monitor"));
                if from < 10 && monitored {
                    return Err(later(from, "source `monitor`", 10));
                }
                let ordered = body
                    .get("canvases")
                    .and_then(toml::Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(toml::Value::as_table)
                    .any(|c| c.contains_key("order"));
                if from < 11 && ordered {
                    return Err(later(from, "canvas `order`", 11));
                }
                let glided = body.contains_key("glide")
                    || body
                        .get("sources")
                        .and_then(toml::Value::as_array)
                        .into_iter()
                        .flatten()
                        .filter_map(toml::Value::as_table)
                        .any(|s| s.contains_key("glide"));
                if from < 12 && glided {
                    return Err(later(from, "`glide`", 12));
                }
                let timed = body
                    .get("scenes")
                    .and_then(toml::Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(toml::Value::as_table)
                    .filter_map(|s| s.get("items"))
                    .filter_map(toml::Value::as_array)
                    .flatten()
                    .filter_map(toml::Value::as_table)
                    .any(|i| i.contains_key("show_for_ms"));
                if from < 13 && timed {
                    return Err(later(from, "item `show_for_ms`", 13));
                }
                let regioned = body
                    .get("sources")
                    .and_then(toml::Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(toml::Value::as_table)
                    .any(|s| s.contains_key("region") || s.contains_key("chrome"));
                if from < 14 && regioned {
                    return Err(later(from, "source `region` or `chrome`", 14));
                }
                let layout_space = body
                    .get("canvases")
                    .and_then(toml::Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(toml::Value::as_table)
                    .any(|c| c.contains_key("layout_width") || c.contains_key("layout_height"));
                if from < 15 && layout_space {
                    return Err(later(from, "canvas `layout_width` or `layout_height`", 15));
                }
                let separate = body
                    .get("sources")
                    .and_then(toml::Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(toml::Value::as_table)
                    .any(|s| s.contains_key("record_separately"));
                if from < 16 && separate {
                    return Err(later(from, "source `record_separately`", 16));
                }
                let levels = body
                    .get("scenes")
                    .and_then(toml::Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(toml::Value::as_table)
                    .any(|s| s.contains_key("audio"));
                if from < 17 && levels {
                    return Err(later(from, "scene `audio`", 17));
                }
                let video_as = body
                    .get("sources")
                    .and_then(toml::Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(toml::Value::as_table)
                    .filter_map(|s| s.get("record_separately"))
                    .filter_map(toml::Value::as_table)
                    .any(|r| r.contains_key("video_as"));
                if video_as {
                    return Err(later(from, "`record_separately.video_as`", 18));
                }
                Ok(())
            }
            _ => Err(BodyError::NoUpgrade(from)),
        }
    }

    fn to_body(&self) -> Result<toml::Table, BodyError> {
        let n = self.id_counters();
        let glide = (!self.glide().is_default()).then(|| GlideDoc::from(self.glide()));
        let transition =
            (!self.transition().is_default()).then(|| TransitionDoc::from(self.transition()));
        let canvases = self
            .canvases()
            .map(|c| {
                let r = c.frame_rate.per_second();
                CanvasDoc {
                    id: c.id.get(),
                    name: c.name.clone(),
                    width: c.size.width,
                    height: c.size.height,
                    frame_rate: [r.num(), r.den()],
                    program: c.program.map(SceneId::get),
                    preview: c.preview.map(SceneId::get),
                    globals: c.globals.map(SceneId::get),
                    master: (!c.master.is_default()).then(|| MasterDoc {
                        volume: f64::from(c.master.volume),
                        muted: c.master.muted,
                    }),
                    follow: c.follow.map(CanvasId::get),
                    fallback: match (c.follow, c.fallback) {
                        (Some(_), Fallback::Scene(id)) => {
                            Some(FallbackDoc::Scene { scene: id.get() })
                        }
                        _ => None,
                    },
                    order: c.order.iter().map(|id| id.get()).collect(),
                    layout_width: (c.layout_size != c.size).then_some(c.layout_size.width),
                    layout_height: (c.layout_size != c.size).then_some(c.layout_size.height),
                }
            })
            .collect();
        let sources = self
            .sources()
            .map(|s| {
                Ok(SourceDoc {
                    id: s.id.get(),
                    name: s.name.clone(),
                    type_id: s.type_id.as_str().to_owned(),
                    settings: settings_doc(&s.settings)?,
                    filters: s
                        .filters
                        .iter()
                        .map(|f| {
                            Ok(FilterDoc {
                                id: f.id.get(),
                                type_id: f.type_id.as_str().to_owned(),
                                enabled: f.enabled,
                                settings: settings_doc(&f.settings)?,
                            })
                        })
                        .collect::<Result<_, BodyError>>()?,
                    volume: wide(s.audio.volume),
                    muted: s.audio.muted,
                    sync_offset_ms: s.audio.sync_offset_ms,
                    mixer_hidden: s.audio.mixer_hidden,
                    monitor: s.audio.monitor.into(),
                    glide: s.glide.map(GlideDoc::from),
                    hints: s.hints.clone(),
                    region: s.region.map(SourceRegion::to_array),
                    chrome: s.chrome.map(ChromeDoc::from),
                    record_separately: (!s.record_separately.is_none())
                        .then(|| RecordSeparatelyDoc::from(s.record_separately)),
                    activity: ActivityDoc::from(s.activity),
                })
            })
            .collect::<Result<_, BodyError>>()?;
        let scenes = self
            .scenes()
            .map(|s| {
                let canvas = match s.scope {
                    SceneScope::Shared => None,
                    SceneScope::Canvas(c) => Some(c.get()),
                    _ => {
                        return Err(BodyError::Value(format!(
                            "{} has a scope documents cannot hold",
                            s.id
                        )));
                    }
                };
                let items = s
                    .items
                    .iter()
                    .map(|i| {
                        let (source, scene) = match i.content {
                            ItemContent::Source(id) => (Some(id.get()), None),
                            ItemContent::Scene(id) => (None, Some(id.get())),
                            _ => {
                                return Err(BodyError::Value(format!(
                                    "{} places content documents cannot hold",
                                    i.id
                                )));
                            }
                        };
                        Ok(ItemDoc {
                            id: i.id.get(),
                            source,
                            scene,
                            visible: i.visible,
                            locked: i.locked,
                            show_for_ms: i.show_for_ms,
                            layout: LayoutDoc::of(&i.layout),
                            overrides: i
                                .overrides
                                .iter()
                                .map(|(c, l)| OverrideDoc {
                                    canvas: c.get(),
                                    layout: LayoutDoc::of(l),
                                })
                                .collect(),
                        })
                    })
                    .collect::<Result<_, BodyError>>()?;
                Ok(SceneDoc {
                    id: s.id.get(),
                    name: s.name.clone(),
                    canvas,
                    items,
                    audio: s.audio.as_ref().map(SceneAudioDoc::of),
                    transition_in: s.transition_in.map(SceneTransitionDoc::of),
                })
            })
            .collect::<Result<_, BodyError>>()?;
        document::body_of(&Body {
            next: Next {
                canvas: n.canvas,
                source: n.source,
                scene: n.scene,
                item: n.item,
                filter: n.filter,
            },
            glide,
            transition,
            canvases,
            sources,
            scenes,
        })
    }

    fn from_body(body: toml::Table) -> Result<Self, BodyError> {
        let b: Body = document::from_body(body)?;
        let canvases = b
            .canvases
            .into_iter()
            .map(|c| {
                let [num, den] = c.frame_rate;
                let rate = FrameRate::new(num, den).ok_or_else(|| {
                    BodyError::Value(format!("canvas {:?}: frame rate {num}/{den}", c.name))
                })?;
                let mut canvas = Canvas::new(
                    CanvasId::from_raw(c.id),
                    &c.name,
                    Size::new(c.width, c.height),
                    rate,
                );
                canvas.program = c.program.map(SceneId::from_raw);
                canvas.preview = c.preview.map(SceneId::from_raw);
                canvas.globals = c.globals.map(SceneId::from_raw);
                if let Some(m) = c.master {
                    canvas.master = MasterAudio::new(m.volume as f32, m.muted);
                }
                canvas.follow = c.follow.map(CanvasId::from_raw);
                canvas.fallback = match c.fallback {
                    Some(FallbackDoc::Scene { scene }) => Fallback::Scene(SceneId::from_raw(scene)),
                    _ => Fallback::KeepLast,
                };
                canvas.order = c.order.into_iter().map(SceneId::from_raw).collect();
                canvas.layout_size = match (c.layout_width, c.layout_height) {
                    (None, None) => canvas.size,
                    (Some(w), Some(h)) => Size::new(w, h),
                    _ => {
                        return Err(BodyError::Value(format!(
                            "canvas {:?}: layout_width and layout_height are set together",
                            c.name
                        )));
                    }
                };
                Ok(canvas)
            })
            .collect::<Result<Vec<_>, BodyError>>()?;
        let sources = b
            .sources
            .into_iter()
            .map(|s| {
                let mut source = Source::new(
                    SourceId::from_raw(s.id),
                    &s.name,
                    NodeTypeId::new(s.type_id),
                    settings(s.settings),
                );
                source.filters = s
                    .filters
                    .into_iter()
                    .map(|f| {
                        let mut filter = Filter::new(
                            FilterId::from_raw(f.id),
                            NodeTypeId::new(f.type_id),
                            settings(f.settings),
                        );
                        filter.enabled = f.enabled;
                        filter
                    })
                    .collect();
                source.audio = SourceAudio::new(s.volume as f32, s.muted)
                    .with_sync_offset(s.sync_offset_ms)
                    .with_mixer_hidden(s.mixer_hidden)
                    .with_monitor(s.monitor.into());
                source.glide = s.glide.map(SourceGlide::from);
                source.hints = s.hints;
                source.region = s.region.map(SourceRegion::from_array);
                source.chrome = s.chrome.map(|chrome| chrome.chrome()).transpose()?;
                source.record_separately = s
                    .record_separately
                    .as_ref()
                    .map_or(Ok(RecordSeparately::default()), RecordSeparatelyDoc::flag)?;
                source.activity = s
                    .activity
                    .as_ref()
                    .map(|a| a.activity(&s.name))
                    .transpose()?
                    .unwrap_or_default();
                Ok(source)
            })
            .collect::<Result<Vec<_>, BodyError>>()?;
        let scenes = b
            .scenes
            .into_iter()
            .map(|s| {
                let scope = s.canvas.map_or(SceneScope::Shared, |c| {
                    SceneScope::Canvas(CanvasId::from_raw(c))
                });
                let audio = s.audio.map(|a| a.audio(&s.name)).transpose()?;
                let transition_in = s
                    .transition_in
                    .map(|t| t.transition(&s.name))
                    .transpose()?
                    .flatten();
                let mut scene =
                    Scene::new(SceneId::from_raw(s.id), &s.name, scope).with_audio(audio);
                scene.transition_in = transition_in;
                scene.items = s
                    .items
                    .into_iter()
                    .map(|i| {
                        let content = match (i.source, i.scene) {
                            (Some(id), None) => ItemContent::Source(SourceId::from_raw(id)),
                            (None, Some(id)) => ItemContent::Scene(SceneId::from_raw(id)),
                            _ => {
                                return Err(BodyError::Value(format!(
                                    "item {} in scene {:?} must have either source or scene",
                                    i.id, s.name
                                )));
                            }
                        };
                        let mut item =
                            SceneItem::new(ItemId::from_raw(i.id), content, i.layout.layout()?);
                        item.visible = i.visible;
                        item.locked = i.locked;
                        item.show_for_ms = i.show_for_ms;
                        for o in i.overrides {
                            let canvas = CanvasId::from_raw(o.canvas);
                            if item.overrides.insert(canvas, o.layout.layout()?).is_some() {
                                return Err(BodyError::Value(format!(
                                    "item {} has two layouts for canvas {}",
                                    i.id, o.canvas
                                )));
                            }
                        }
                        Ok(item)
                    })
                    .collect::<Result<_, BodyError>>()?;
                Ok(scene)
            })
            .collect::<Result<Vec<_>, BodyError>>()?;
        let n = b.next;
        let glide = b.glide.map(CollectionGlide::from).unwrap_or_default();
        let transition = b
            .transition
            .map(CollectionTransition::try_from)
            .transpose()?
            .unwrap_or_default();
        SceneCollection::from_parts_with_transition(
            canvases,
            sources,
            scenes,
            IdCounters {
                canvas: n.canvas,
                source: n.source,
                scene: n.scene,
                item: n.item,
                filter: n.filter,
            },
            glide,
            transition,
        )
        .map_err(BodyError::Scene)
    }
}
