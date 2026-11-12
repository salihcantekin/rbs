//! The scene collection document (glossary: Scene collection): the on-disk form of
//! [`rbs_scene::SceneCollection`], mapping one-to-one onto the model (ADR-0021).
//!
//! ```toml
//! format = 12
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
//!
//! [[scenes]]
//! id = 0
//! name = "Scene"
//! # canvas = 0                 # only that canvas shows it; absent: shared
//!
//! [[scenes.items]]
//! id = 0
//! source = 0                   # or: scene = <id> for a nested scene
//! layout = { position = [0.0, 0.0], size = [1920.0, 1080.0] }
//! ```
//!
//! Layouts may also have `rotation` (degrees), `crop` (`[left, top, right, bottom]`
//! source pixels), `opacity` and `style` (ADR-0053, format 2: `shape = "rectangle" |
//! "circle" | "ellipse"`, `radius` for rectangles, `border = { width, color =
//! "#rrggbbaa" }`, `shadow = { offset = [x, y], blur, color }`); items `visible = false`,
//! `locked = true` (format 3) and `overrides = [{ canvas = 1, layout = {...} }]`; sources `filters = [{ id, type,
//! enabled, settings }]`, `volume` (0 to 1, default 1) and `muted = true` (format 3,
//! ADR-0059), `sync_offset_ms` (-200 to 200, default 0; format 4, ADR-0062), `hints = { target_app = "...", target_title = "..." }` (format 8, ADR-0077). Setting values are TOML booleans, integers, floats or
//! strings. Loading validates the whole collection
//! ([`SceneCollection::from_parts`]).

use std::collections::BTreeMap;

use rbs_core::media::Size;
use rbs_core::timing::FrameRate;
use rbs_graph::node::NodeTypeId;
use rbs_scene::model::{
    Border, Canvas, CanvasId, CollectionGlide, Crop, Fallback, Filter, FilterId, GlideEasing,
    ItemContent, ItemId, ItemStyle, Layout, MasterAudio, Scene, SceneId, SceneItem, SceneScope,
    SettingValue, Settings, Shadow, Shape, Source, SourceAudio, SourceGlide, SourceId,
    SourceMonitor,
};
use rbs_scene::{IdCounters, SceneCollection};
use serde::{Deserialize, Serialize};

use crate::document::{self, Document};

#[derive(Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct Body {
    #[serde(default)]
    next: Next,
    /// Collection morph (format 12, ADR-0100); absent: defaults.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    glide: Option<GlideDoc>,
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
}

#[derive(Serialize, Deserialize, Default, Clone, Copy, PartialEq)]
#[serde(rename_all = "lowercase")]
enum ShapeName {
    #[default]
    Rectangle,
    Circle,
    Ellipse,
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
fn color(text: &str) -> Result<[u8; 4], String> {
    let bad = || format!("colour {text:?} is not #rrggbb or #rrggbbaa");
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
            // Shapes this build does not know cannot exist in its model.
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
    fn style(&self) -> Result<ItemStyle, String> {
        let shape = match self.shape {
            ShapeName::Rectangle => Shape::Rectangle {
                radius: self.radius as f32,
            },
            _ if self.radius != 0.0 => {
                return Err("radius is only for shape = \"rectangle\"".into());
            }
            ShapeName::Circle => Shape::Circle,
            ShapeName::Ellipse => Shape::Ellipse,
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
        }
    }

    fn layout(&self) -> Result<Layout, String> {
        let [left, top, right, bottom] = self.crop;
        Ok(
            Layout::new(self.position.map(|v| v as f32), self.size.map(|v| v as f32))
                .with_rotation(self.rotation as f32)
                .with_crop(Crop::new(left, top, right, bottom))
                .with_opacity(self.opacity as f32)
                .with_style(self.style.style()?),
        )
    }
}

fn settings_doc(s: &Settings) -> Result<BTreeMap<String, Value>, String> {
    s.iter()
        .map(|(k, v)| {
            let v = match v {
                SettingValue::Bool(b) => Value::Bool(*b),
                SettingValue::Int(i) => Value::Int(*i),
                SettingValue::Float(f) => Value::Float(*f),
                SettingValue::Text(t) => Value::Text(t.clone()),
                _ => return Err(format!("setting {k:?} has a type documents cannot hold")),
            };
            Ok((k.clone(), v))
        })
        .collect()
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

impl Document for SceneCollection {
    const KIND: &'static str = "scene-collection";
    const FORMAT: u32 = 12;

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
    /// is a format 12 body without them.
    fn upgrade(from: u32, body: &mut toml::Table) -> Result<(), String> {
        match from {
            1..=11 => {
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
                    return Err(format!(
                        "format {from} has no canvas `follow` or `fallback` (format 9)"
                    ));
                }
                let monitored = body
                    .get("sources")
                    .and_then(toml::Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(toml::Value::as_table)
                    .any(|s| s.contains_key("monitor"));
                if from < 10 && monitored {
                    return Err(format!("format {from} has no source `monitor` (format 10)"));
                }
                let ordered = body
                    .get("canvases")
                    .and_then(toml::Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(toml::Value::as_table)
                    .any(|c| c.contains_key("order"));
                if from < 11 && ordered {
                    return Err(format!("format {from} has no canvas `order` (format 11)"));
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
                    return Err(format!("format {from} has no `glide` (format 12)"));
                }
                Ok(())
            }
            _ => Err(format!("format {from} cannot be read")),
        }
    }

    fn to_body(&self) -> Result<toml::Table, String> {
        let n = self.id_counters();
        let glide = (!self.glide().is_default()).then(|| GlideDoc::from(self.glide()));
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
                        .collect::<Result<_, String>>()?,
                    volume: wide(s.audio.volume),
                    muted: s.audio.muted,
                    sync_offset_ms: s.audio.sync_offset_ms,
                    mixer_hidden: s.audio.mixer_hidden,
                    monitor: s.audio.monitor.into(),
                    glide: s.glide.map(GlideDoc::from),
                    hints: s.hints.clone(),
                })
            })
            .collect::<Result<_, String>>()?;
        let scenes = self
            .scenes()
            .map(|s| {
                let canvas = match s.scope {
                    SceneScope::Shared => None,
                    SceneScope::Canvas(c) => Some(c.get()),
                    _ => return Err(format!("{} has a scope documents cannot hold", s.id)),
                };
                let items = s
                    .items
                    .iter()
                    .map(|i| {
                        let (source, scene) = match i.content {
                            ItemContent::Source(id) => (Some(id.get()), None),
                            ItemContent::Scene(id) => (None, Some(id.get())),
                            _ => {
                                return Err(format!(
                                    "{} places content documents cannot hold",
                                    i.id
                                ));
                            }
                        };
                        Ok(ItemDoc {
                            id: i.id.get(),
                            source,
                            scene,
                            visible: i.visible,
                            locked: i.locked,
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
                    .collect::<Result<_, String>>()?;
                Ok(SceneDoc {
                    id: s.id.get(),
                    name: s.name.clone(),
                    canvas,
                    items,
                })
            })
            .collect::<Result<_, String>>()?;
        document::body_of(&Body {
            next: Next {
                canvas: n.canvas,
                source: n.source,
                scene: n.scene,
                item: n.item,
                filter: n.filter,
            },
            glide,
            canvases,
            sources,
            scenes,
        })
    }

    fn from_body(body: toml::Table) -> Result<Self, String> {
        let b: Body = document::from_body(body)?;
        let canvases = b
            .canvases
            .into_iter()
            .map(|c| {
                let [num, den] = c.frame_rate;
                let rate = FrameRate::new(num, den)
                    .ok_or_else(|| format!("canvas {:?}: frame rate {num}/{den}", c.name))?;
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
                Ok(canvas)
            })
            .collect::<Result<Vec<_>, String>>()?;
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
                source
            })
            .collect();
        let scenes = b
            .scenes
            .into_iter()
            .map(|s| {
                let scope = s.canvas.map_or(SceneScope::Shared, |c| {
                    SceneScope::Canvas(CanvasId::from_raw(c))
                });
                let mut scene = Scene::new(SceneId::from_raw(s.id), &s.name, scope);
                scene.items = s
                    .items
                    .into_iter()
                    .map(|i| {
                        let content = match (i.source, i.scene) {
                            (Some(id), None) => ItemContent::Source(SourceId::from_raw(id)),
                            (None, Some(id)) => ItemContent::Scene(SceneId::from_raw(id)),
                            _ => {
                                return Err(format!(
                                    "item {} in scene {:?} must have either source or scene",
                                    i.id, s.name
                                ));
                            }
                        };
                        let mut item =
                            SceneItem::new(ItemId::from_raw(i.id), content, i.layout.layout()?);
                        item.visible = i.visible;
                        item.locked = i.locked;
                        for o in i.overrides {
                            let canvas = CanvasId::from_raw(o.canvas);
                            if item.overrides.insert(canvas, o.layout.layout()?).is_some() {
                                return Err(format!(
                                    "item {} has two layouts for canvas {}",
                                    i.id, o.canvas
                                ));
                            }
                        }
                        Ok(item)
                    })
                    .collect::<Result<_, String>>()?;
                Ok(scene)
            })
            .collect::<Result<Vec<_>, String>>()?;
        let n = b.next;
        let glide = b.glide.map(CollectionGlide::from).unwrap_or_default();
        SceneCollection::from_parts_with_glide(
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
        )
        .map_err(|e| e.to_string())
    }
}
