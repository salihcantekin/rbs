//! Lenient views of the OBS scene collection JSON. Unknown keys are ignored and a
//! missing key takes OBS's default (spec `obs-import.md`).

use serde_json::{Map, Value};

/// One OBS source object: from `sources`, `groups` or a top-level audio device key.
#[derive(Debug, Clone)]
pub struct ObsSource {
    /// `id`, the OBS type (`image_source`, `scene`, ...).
    pub id: String,
    /// `versioned_id`, or `id` when absent.
    pub versioned_id: String,
    /// `name`.
    pub name: String,
    /// `uuid`, when OBS stored one (OBS 30+).
    pub uuid: Option<String>,
    /// `settings`.
    pub settings: Map<String, Value>,
    /// `filters`.
    pub filters: Vec<ObsFilter>,
    /// `volume`, linear.
    pub volume: f64,
    /// `muted`.
    pub muted: bool,
    /// `sync`, nanoseconds.
    pub sync_ns: i64,
    /// `monitoring_type`: 0 none, 1 monitor only, 2 monitor and output.
    pub monitoring_type: u64,
    /// `balance`, 0.5 is centre.
    pub balance: f64,
    /// `mixers`/`audio_mixers` track bits.
    pub audio_mixers: u64,
    /// `private_settings`.
    pub private_settings: Map<String, Value>,
    /// `push-to-talk` or `push-to-mute` enabled.
    pub push_to: bool,
}

/// One filter of a source.
#[derive(Debug, Clone)]
pub struct ObsFilter {
    /// `id`.
    pub id: String,
    /// `name`.
    pub name: String,
    /// `enabled`.
    pub enabled: bool,
    /// `settings`.
    pub settings: Map<String, Value>,
}

fn obj(v: Option<&Value>) -> Map<String, Value> {
    v.and_then(Value::as_object).cloned().unwrap_or_default()
}

/// `key` as text, or "".
pub fn text<'a>(m: &'a Map<String, Value>, key: &str) -> &'a str {
    m.get(key).and_then(Value::as_str).unwrap_or("")
}

/// `key` as a number, or `default`.
pub fn num(m: &Map<String, Value>, key: &str, default: f64) -> f64 {
    m.get(key).and_then(Value::as_f64).unwrap_or(default)
}

/// `key` as a flag, or `default`.
pub fn flag(m: &Map<String, Value>, key: &str, default: bool) -> bool {
    m.get(key).and_then(Value::as_bool).unwrap_or(default)
}

/// `key` as an unsigned integer, or `default`. Negative or fractional values count
/// as absent.
pub fn uint(m: &Map<String, Value>, key: &str, default: u64) -> u64 {
    m.get(key).and_then(Value::as_u64).unwrap_or(default)
}

/// `key` as an `{x, y}` pair, or `None`.
pub fn vec2(m: &Map<String, Value>, key: &str) -> Option<[f64; 2]> {
    let v = m.get(key)?.as_object()?;
    Some([v.get("x")?.as_f64()?, v.get("y")?.as_f64()?])
}

impl ObsFilter {
    fn parse(v: &Value) -> Option<Self> {
        let m = v.as_object()?;
        Some(Self {
            id: text(m, "id").to_owned(),
            name: text(m, "name").to_owned(),
            enabled: flag(m, "enabled", true),
            settings: obj(m.get("settings")),
        })
    }
}

impl ObsSource {
    /// A source object; `None` when it is not an object or has no `id`.
    pub fn parse(v: &Value) -> Option<Self> {
        let m = v.as_object()?;
        let id = text(m, "id");
        if id.is_empty() {
            return None;
        }
        let versioned = text(m, "versioned_id");
        Some(Self {
            id: id.to_owned(),
            versioned_id: if versioned.is_empty() { id } else { versioned }.to_owned(),
            name: text(m, "name").to_owned(),
            uuid: m
                .get("uuid")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_owned),
            settings: obj(m.get("settings")),
            filters: m
                .get("filters")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(ObsFilter::parse).collect())
                .unwrap_or_default(),
            volume: num(m, "volume", 1.0),
            muted: flag(m, "muted", false),
            sync_ns: m.get("sync").and_then(Value::as_i64).unwrap_or(0),
            monitoring_type: uint(m, "monitoring_type", 0),
            balance: num(m, "balance", 0.5),
            audio_mixers: m
                .get("mixers")
                .or_else(|| m.get("audio_mixers"))
                .and_then(Value::as_u64)
                .unwrap_or(0xFF),
            private_settings: obj(m.get("private_settings")),
            push_to: flag(m, "push-to-talk", false) || flag(m, "push-to-mute", false),
        })
    }

    /// True for a scene (`scene`).
    pub fn is_scene(&self) -> bool {
        self.id == "scene"
    }

    /// True for a group (`group`).
    pub fn is_group(&self) -> bool {
        self.id == "group"
    }

    /// The scene or group's `settings.items`.
    pub fn items(&self) -> Vec<ObsItem> {
        self.settings
            .get("items")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(ObsItem::parse).collect())
            .unwrap_or_default()
    }
}

/// One item of a scene or group.
#[derive(Debug, Clone)]
pub struct ObsItem {
    /// `name`: the placed source's name.
    pub name: String,
    /// `source_uuid` (OBS 30+).
    pub source_uuid: Option<String>,
    /// `visible`.
    pub visible: bool,
    /// `locked`.
    pub locked: bool,
    /// The raw item object, for the transform and the keys only some items have.
    pub raw: Map<String, Value>,
}

impl ObsItem {
    fn parse(v: &Value) -> Option<Self> {
        let m = v.as_object()?;
        Some(Self {
            name: text(m, "name").to_owned(),
            source_uuid: m
                .get("source_uuid")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_owned),
            visible: flag(m, "visible", true),
            locked: flag(m, "locked", false),
            raw: m.clone(),
        })
    }

    /// True when only OBS 31's relative transform keys are present (decision 8).
    pub fn relative_only(&self) -> bool {
        !self.raw.contains_key("pos")
            && (self.raw.contains_key("pos_rel") || self.raw.contains_key("scale_ref"))
    }
}

/// The top-level keys of OBS's global audio devices, in the order OBS lists them.
pub const GLOBAL_AUDIO_KEYS: [&str; 6] = [
    "DesktopAudioDevice1",
    "DesktopAudioDevice2",
    "AuxAudioDevice1",
    "AuxAudioDevice2",
    "AuxAudioDevice3",
    "AuxAudioDevice4",
];

/// A readable name for an OBS type id, for reports and for sources with no name.
pub fn type_label(id: &str) -> &'static str {
    match id {
        "scene" => "Scene",
        "group" => "Group",
        "monitor_capture"
        | "display_capture"
        | "xshm_input"
        | "pipewire-desktop-capture-source" => "Display capture",
        "screen_capture" => "Screen capture",
        "window_capture" | "xcomposite_input" | "pipewire-window-capture-source" => {
            "Window capture"
        }
        "game_capture" => "Game capture",
        "dshow_input"
        | "av_capture_input"
        | "av_capture_input_v2"
        | "macos-avcapture"
        | "v4l2_input" => "Video capture device",
        "wasapi_input_capture"
        | "coreaudio_input_capture"
        | "pulse_input_capture"
        | "alsa_input_capture" => "Audio input",
        "wasapi_output_capture" | "coreaudio_output_capture" | "pulse_output_capture" => {
            "Audio output"
        }
        "wasapi_process_output_capture" | "sck_audio_capture" => "Application audio",
        "image_source" => "Image",
        "color_source" | "color_source_v2" | "color_source_v3" => "Colour",
        "ffmpeg_source" => "Media source",
        "vlc_source" => "VLC video source",
        "browser_source" => "Browser",
        "text_gdiplus" | "text_gdiplus_v2" | "text_gdiplus_v3" | "text_ft2_source"
        | "text_ft2_source_v2" => "Text",
        "slideshow" | "slideshow_v2" => "Image slide show",
        _ => "Source",
    }
}
