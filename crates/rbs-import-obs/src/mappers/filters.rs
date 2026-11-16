//! Filter mappers of ADR-0131 decision 7.

use rbs_scene::model::{SettingValue, Settings};

use super::{FilterKind, FilterMapper, FilterOutcome, MapContext, obs_color};
use crate::env::clamp_field;
use crate::obs::{ObsFilter, flag, num, text, uint};

/// RBS filter types, as strings checked by `rbs-app` against the catalogues.
pub mod rbs {
    /// Gain.
    pub const GAIN: &str = "org.rbs.audio-filter.gain";
    /// Noise gate.
    pub const NOISE_GATE: &str = "org.rbs.audio-filter.noise-gate";
    /// Ducker.
    pub const DUCKER: &str = "org.rbs.audio-filter.ducker";
    /// Chroma key.
    pub const CHROMA_KEY: &str = "org.rbs.filter.chroma-key";
    /// Colour correction.
    pub const COLOR_CORRECTION: &str = "org.rbs.filter.color-correction";
    /// Sharpen.
    pub const SHARPEN: &str = "org.rbs.filter.sharpen";

    /// Every filter type a mapper may produce, with the keys it writes.
    pub const ALL: [(&str, &[&str]); 6] = [
        (GAIN, &["gain"]),
        (
            NOISE_GATE,
            &[
                "open_threshold",
                "close_threshold",
                "attack",
                "hold",
                "release",
            ],
        ),
        (DUCKER, &["key_source", "threshold", "attack", "release"]),
        (
            CHROMA_KEY,
            &["key_color", "similarity", "smoothness", "spill"],
        ),
        (
            COLOR_CORRECTION,
            &["brightness", "contrast", "saturation", "gamma", "hue"],
        ),
        (SHARPEN, &["amount"]),
    ];
}

/// OBS audio filter ids, so an unmapped one is "not chosen" when audio filters are
/// left out in the wizard.
pub const OBS_AUDIO_FILTERS: [&str; 13] = [
    "gain_filter",
    "noise_gate_filter",
    "compressor_filter",
    "limiter_filter",
    "expander_filter",
    "upward_compressor_filter",
    "noise_suppress_filter",
    "noise_suppress_filter_v2",
    "basic_eq_filter",
    "invert_polarity_filter",
    "vst_filter",
    "audio_monitor",
    "async_delay_filter_audio",
];

/// Every built-in filter mapper.
pub fn builtin() -> Vec<Box<dyn FilterMapper>> {
    vec![
        Box::new(Gain),
        Box::new(Gate),
        Box::new(Compressor),
        Box::new(ChromaKey("chroma_key_filter")),
        Box::new(ChromaKey("chroma_key_filter_v2")),
        Box::new(ColorCorrection("color_filter")),
        Box::new(ColorCorrection("color_filter_v2")),
        Box::new(Sharpen("sharpness_filter")),
        Box::new(Sharpen("sharpness_filter_v2")),
        Box::new(CropFilter),
    ]
}

/// Builds a filter's settings, clamping each number to its field.
struct Builder<'c> {
    type_id: &'static str,
    fields: Option<&'static [rbs_params::Field]>,
    settings: Settings,
    notes: Vec<String>,
    clamped: Vec<&'static str>,
    _ctx: std::marker::PhantomData<&'c ()>,
}

impl Builder<'_> {
    fn new(type_id: &'static str, ctx: &MapContext<'_>) -> Self {
        Self {
            type_id,
            fields: ctx.fields(type_id),
            settings: Settings::new(),
            notes: Vec::new(),
            clamped: Vec::new(),
            _ctx: std::marker::PhantomData,
        }
    }

    fn number(&mut self, key: &'static str, value: f64) {
        let (v, clamped) = clamp_field(self.fields, key, value);
        if clamped {
            self.clamped.push(key);
        }
        self.settings.insert(key.to_owned(), SettingValue::Float(v));
    }

    fn note(&mut self, note: &str) {
        self.notes.push(note.to_owned());
    }

    fn done(mut self, source_refs: Vec<(String, String)>) -> FilterOutcome {
        if !self.clamped.is_empty() {
            self.notes.push(format!(
                "Out of RBS's range, so limited: {}.",
                self.clamped.join(", ")
            ));
        }
        FilterOutcome::Filter {
            type_id: self.type_id,
            settings: self.settings,
            source_refs,
            notes: self.notes,
        }
    }
}

struct Gain;

impl FilterMapper for Gain {
    fn obs_id(&self) -> &'static str {
        "gain_filter"
    }
    fn rbs_types(&self) -> &'static [&'static str] {
        &[rbs::GAIN]
    }
    fn kind(&self) -> FilterKind {
        FilterKind::Audio
    }
    fn map(&self, f: &ObsFilter, ctx: &mut MapContext<'_>) -> FilterOutcome {
        let mut b = Builder::new(rbs::GAIN, ctx);
        b.number("gain", num(&f.settings, "db", 0.0));
        b.done(Vec::new())
    }
}

struct Gate;

impl FilterMapper for Gate {
    fn obs_id(&self) -> &'static str {
        "noise_gate_filter"
    }
    fn rbs_types(&self) -> &'static [&'static str] {
        &[rbs::NOISE_GATE]
    }
    fn kind(&self) -> FilterKind {
        FilterKind::Audio
    }
    fn map(&self, f: &ObsFilter, ctx: &mut MapContext<'_>) -> FilterOutcome {
        let s = &f.settings;
        let mut b = Builder::new(rbs::NOISE_GATE, ctx);
        b.number("open_threshold", num(s, "open_threshold", -26.0));
        b.number("close_threshold", num(s, "close_threshold", -32.0));
        b.number("attack", num(s, "attack_time", 25.0));
        b.number("hold", num(s, "hold_time", 200.0));
        b.number("release", num(s, "release_time", 150.0));
        b.note("OBS fades the gate in amplitude, RBS in decibels, so it opens and closes a little differently.");
        b.done(Vec::new())
    }
}

/// A compressor with a sidechain source is how OBS users duck music under a voice.
struct Compressor;

impl FilterMapper for Compressor {
    fn obs_id(&self) -> &'static str {
        "compressor_filter"
    }
    fn rbs_types(&self) -> &'static [&'static str] {
        &[rbs::DUCKER]
    }
    fn kind(&self) -> FilterKind {
        FilterKind::Audio
    }
    fn map(&self, f: &ObsFilter, ctx: &mut MapContext<'_>) -> FilterOutcome {
        let s = &f.settings;
        let key = text(s, "sidechain_source");
        if key.is_empty() || key == "none" {
            return FilterOutcome::Skipped {
                reason: "A compressor is not in RBS yet.".to_owned(),
            };
        }
        let mut b = Builder::new(rbs::DUCKER, ctx);
        b.number("threshold", num(s, "threshold", -18.0));
        b.number("attack", num(s, "attack_time", 6.0));
        b.number("release", num(s, "release_time", 60.0));
        b.note(
            "The compressor with a sidechain became a Ducker; check how far it lowers the sound.",
        );
        b.done(vec![("key_source".to_owned(), key.to_owned())])
    }
}

struct ChromaKey(&'static str);

impl FilterMapper for ChromaKey {
    fn obs_id(&self) -> &'static str {
        self.0
    }
    fn rbs_types(&self) -> &'static [&'static str] {
        &[rbs::CHROMA_KEY]
    }
    fn kind(&self) -> FilterKind {
        FilterKind::Video
    }
    fn map(&self, f: &ObsFilter, ctx: &mut MapContext<'_>) -> FilterOutcome {
        let s = &f.settings;
        let mut b = Builder::new(rbs::CHROMA_KEY, ctx);
        let color = match text(s, "key_color_type") {
            "blue" => "#0000ff".to_owned(),
            "magenta" => "#ff00ff".to_owned(),
            "custom" => obs_color(uint(s, "key_color", 0xFF00_FF00) | 0xFF00_0000),
            _ => "#00ff00".to_owned(),
        };
        b.settings
            .insert("key_color".to_owned(), SettingValue::Text(color));
        b.number("similarity", num(s, "similarity", 400.0) / 1000.0);
        b.number("smoothness", num(s, "smoothness", 80.0) / 1000.0);
        b.number("spill", num(s, "spill", 100.0) / 1000.0);
        let opacity_default = if self.0 == "chroma_key_filter" {
            100.0
        } else {
            1.0
        };
        if num(s, "opacity", opacity_default) != opacity_default
            || num(s, "contrast", 0.0) != 0.0
            || num(s, "brightness", 0.0) != 0.0
            || num(s, "gamma", 0.0) != 0.0
        {
            b.note("The key's opacity, contrast, brightness and gamma are not in RBS.");
        }
        b.done(Vec::new())
    }
}

struct ColorCorrection(&'static str);

impl FilterMapper for ColorCorrection {
    fn obs_id(&self) -> &'static str {
        self.0
    }
    fn rbs_types(&self) -> &'static [&'static str] {
        &[rbs::COLOR_CORRECTION]
    }
    fn kind(&self) -> FilterKind {
        FilterKind::Video
    }
    fn map(&self, f: &ObsFilter, ctx: &mut MapContext<'_>) -> FilterOutcome {
        let s = &f.settings;
        let mut b = Builder::new(rbs::COLOR_CORRECTION, ctx);
        b.number("brightness", num(s, "brightness", 0.0));
        b.number("contrast", num(s, "contrast", 0.0));
        b.number("saturation", num(s, "saturation", 0.0) + 1.0);
        b.number("hue", num(s, "hue_shift", 0.0));
        // OBS raises a channel to 1/(1+g) for g >= 0 and to 1-g below; RBS to 1/gamma.
        let g = num(s, "gamma", 0.0);
        b.number("gamma", if g >= 0.0 { 1.0 + g } else { 1.0 / (1.0 - g) });
        let (opacity_default, opacity) = if self.0 == "color_filter" {
            (100.0, num(s, "opacity", 100.0))
        } else {
            (1.0, num(s, "opacity", 1.0))
        };
        if opacity != opacity_default {
            b.note("The filter's opacity is not in RBS.");
        }
        let add = uint(s, "color_add", 0) & 0x00FF_FFFF;
        let mul = uint(s, "color_multiply", 0xFFFF_FFFF) & 0x00FF_FFFF;
        let color = uint(s, "color", 0xFFFF_FFFF) & 0x00FF_FFFF;
        if add != 0 || mul != 0x00FF_FFFF || color != 0x00FF_FFFF {
            b.note("Adding or multiplying a colour is not in RBS.");
        }
        b.note("OBS and RBS compute colour correction slightly differently.");
        b.done(Vec::new())
    }
}

struct Sharpen(&'static str);

impl FilterMapper for Sharpen {
    fn obs_id(&self) -> &'static str {
        self.0
    }
    fn rbs_types(&self) -> &'static [&'static str] {
        &[rbs::SHARPEN]
    }
    fn kind(&self) -> FilterKind {
        FilterKind::Video
    }
    fn map(&self, f: &ObsFilter, ctx: &mut MapContext<'_>) -> FilterOutcome {
        let mut b = Builder::new(rbs::SHARPEN, ctx);
        b.number("amount", num(&f.settings, "sharpness", 0.08) * 2.0);
        b.note("OBS and RBS sharpen slightly differently.");
        b.done(Vec::new())
    }
}

/// `crop_filter` becomes the source's region (ADR-0103); the planner places it.
struct CropFilter;

impl FilterMapper for CropFilter {
    fn obs_id(&self) -> &'static str {
        "crop_filter"
    }
    fn rbs_types(&self) -> &'static [&'static str] {
        &[]
    }
    fn kind(&self) -> FilterKind {
        FilterKind::Video
    }
    fn map(&self, f: &ObsFilter, _ctx: &mut MapContext<'_>) -> FilterOutcome {
        let s = &f.settings;
        let px = |key: &str| {
            s.get(key)
                .and_then(serde_json::Value::as_i64)
                .unwrap_or(0)
                .clamp(0, i64::from(u32::MAX)) as u32
        };
        if flag(s, "relative", true) {
            FilterOutcome::Region {
                left: px("left"),
                top: px("top"),
                right: Some(px("right")),
                bottom: Some(px("bottom")),
                size: None,
            }
        } else {
            FilterOutcome::Region {
                left: px("left"),
                top: px("top"),
                right: None,
                bottom: None,
                size: Some((px("cx"), px("cy"))),
            }
        }
    }
}
