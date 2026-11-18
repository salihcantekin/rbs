//! Text source settings (ADR-0129 decisions 3 and 11, spec `scene-sources.md`, "Text
//! source"): strict checks for edits, lenient reading for documents, and which text
//! sources are still.

use rbs_scene::model::{SettingValue, Settings};
use rbs_text::{
    Align, Fill, LETTER_SPACING_RANGE, LINE_SPACING_RANGE, MAX_CHARS, MAX_PADDING, MAX_PIXELS,
    MAX_SHADOW_OFFSET, MAX_SIDE, MAX_STROKE, Outline, Overflow, SIZE_RANGE, Shadow, TextBlock,
    VAlign, WEIGHT_RANGE,
};

use crate::registry::SourceError;
use crate::still::parse_color;
use crate::types::keys;

/// The box a text source gets when its settings name none (ADR-0129 decision 4).
pub const DEFAULT_BOX: [u32; 2] = [640, 160];

/// Every key a text source uses.
pub const TEXT_KEYS: &[&str] = &[
    keys::KIND,
    keys::TEXT,
    keys::FILE,
    keys::FILE_LINES,
    keys::FONT,
    keys::SIZE,
    keys::WEIGHT,
    keys::ITALIC,
    keys::COLOR,
    keys::COLOR2,
    keys::ANGLE,
    keys::WIDTH,
    keys::HEIGHT,
    keys::ALIGN,
    keys::VALIGN,
    keys::WRAP,
    keys::SINGLE_LINE,
    keys::OVERFLOW,
    keys::LINE_SPACING,
    keys::LETTER_SPACING,
    keys::OUTLINE_WIDTH,
    keys::OUTLINE_COLOR,
    keys::SHADOW_COLOR,
    keys::SHADOW_X,
    keys::SHADOW_Y,
    keys::SHADOW_BLUR,
    keys::BACKGROUND,
    keys::PADDING,
    keys::FIELDS,
    keys::FORMAT,
    keys::TARGET,
    keys::DURATION_MS,
    keys::END_TEXT,
];

/// What a text source shows (ADR-0129 decision 11).
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextKind {
    /// The `text` or `file` content.
    Text,
    /// Local time and date.
    Clock,
    /// Counts down to a time of day or for a duration.
    Countdown,
    /// Counts up while started.
    Stopwatch,
    /// How long the main recording has run.
    RecordingTime,
    /// How long the main stream has run.
    StreamTime,
}

impl TextKind {
    /// The stored name.
    pub fn as_str(self) -> &'static str {
        match self {
            TextKind::Text => "text",
            TextKind::Clock => "clock",
            TextKind::Countdown => "countdown",
            TextKind::Stopwatch => "stopwatch",
            TextKind::RecordingTime => "recording_time",
            TextKind::StreamTime => "stream_time",
        }
    }

    /// The kind of a stored name; `None` for a name this build does not know.
    pub fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "text" => TextKind::Text,
            "clock" => TextKind::Clock,
            "countdown" => TextKind::Countdown,
            "stopwatch" => TextKind::Stopwatch,
            "recording_time" => TextKind::RecordingTime,
            "stream_time" => TextKind::StreamTime,
            _ => return None,
        })
    }
}

/// A text source's settings, checked.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq)]
pub struct TextSettings {
    /// What it shows.
    pub kind: TextKind,
    /// The block to draw (its `text` is the `text` setting; files and live kinds
    /// replace it before drawing).
    pub block: TextBlock,
    /// A local file to show instead of `text`.
    pub file: Option<String>,
    /// 0 = the whole file, N = its last N lines.
    pub file_lines: u32,
    /// `{time}`, `{date}`, ... fields in `text`.
    pub fields: bool,
    /// The clock or timer pattern, if set.
    pub format: Option<String>,
    /// A countdown's time of day, if set.
    pub target: Option<String>,
    /// A countdown's length, if set.
    pub duration_ms: Option<u64>,
    /// Shown at the end of a countdown or while a recording or stream is stopped.
    pub end_text: Option<String>,
    /// One line, as wide as the text (ADR-0150 decision 10): line breaks become
    /// spaces, nothing wraps, and the picture's width follows the text up to the caps
    /// ([`single_line_block`]); `width` is not used.
    pub single_line: bool,
}

impl TextSettings {
    /// Whether the picture changes only when the settings or the file change.
    pub fn is_still(&self) -> bool {
        self.kind == TextKind::Text && !self.fields
    }
}

/// Whether the picture of a text source with `settings` changes only when its settings
/// or file change (ADR-0129 decision 1): kind `text` (or absent) with `fields` off. A
/// kind this build does not know is not still.
pub fn text_is_still(settings: &Settings) -> bool {
    let kind_is_text = match settings.get(keys::KIND) {
        None => true,
        Some(SettingValue::Text(k)) => k == "text",
        Some(_) => false,
    };
    let fields = matches!(settings.get(keys::FIELDS), Some(SettingValue::Bool(true)));
    kind_is_text && !fields
}

/// The text sources of the on-air set (ADR-0129 decision 7): every item, visible or
/// hidden, of each canvas's program, preview and globals scenes and of `scenes` (hold
/// scenes), nested scenes included, plus `sources` (separate picture tracks of a
/// running take, open settings windows) that are text sources.
pub fn text_on_air(
    collection: &rbs_scene::SceneCollection,
    scenes: impl IntoIterator<Item = rbs_scene::model::SceneId>,
    sources: impl IntoIterator<Item = rbs_scene::model::SourceId>,
) -> std::collections::BTreeSet<rbs_scene::model::SourceId> {
    use rbs_scene::model::ItemContent;
    let mut todo: Vec<rbs_scene::model::SceneId> = collection
        .canvases()
        .flat_map(|c| [c.program, c.preview, c.globals])
        .flatten()
        .chain(scenes)
        .collect();
    let mut seen = std::collections::BTreeSet::new();
    let mut out: std::collections::BTreeSet<_> = sources.into_iter().collect();
    while let Some(id) = todo.pop() {
        if !seen.insert(id) {
            continue;
        }
        let Some(scene) = collection.scene(id) else {
            continue;
        };
        for item in &scene.items {
            match item.content {
                ItemContent::Source(s) => {
                    out.insert(s);
                }
                ItemContent::Scene(inner) => todo.push(inner),
                _ => {}
            }
        }
    }
    out.retain(|s| {
        collection.source(*s).is_some_and(|src| {
            crate::types::class_of(&src.type_id) == Some(crate::SourceClass::Text)
        })
    });
    out
}

/// Whether `text` has a character usually drawn as a colour glyph (emoji and
/// pictographs), so the host prefers a renderer with `text.color_glyphs`.
pub fn has_color_glyphs(text: &str) -> bool {
    text.chars().any(|c| {
        matches!(u32::from(c),
            0x1F000..=0x1FAFF | 0x2600..=0x27BF | 0x2B00..=0x2BFF | 0xFE0F)
    })
}

/// The renderer for `block` (ADR-0129 decision 2): the first of `renderers` that
/// draws colour glyphs when the text has emoji, else the first.
pub fn pick_renderer(
    renderers: &[std::sync::Arc<dyn rbs_text::TextRenderer>],
    block: &TextBlock,
) -> Option<std::sync::Arc<dyn rbs_text::TextRenderer>> {
    let color = rbs_core::capability::CapabilityValue::Flag(true);
    let with_color = || {
        renderers.iter().find(|r| {
            r.capabilities()
                .get(&rbs_text::caps::COLOR_GLYPHS)
                .is_some_and(|v| *v == color)
        })
    };
    has_color_glyphs(&block.text)
        .then(with_color)
        .flatten()
        .or_else(|| renderers.first())
        .cloned()
}

/// Checks `settings` strictly, as an edit from the app or the API is checked: only
/// text keys, each of the right kind and in range, and the box given.
pub fn validate_text(settings: &Settings) -> Result<TextSettings, SourceError> {
    if settings.keys().any(|k| !TEXT_KEYS.contains(&k.as_str())) {
        return Err(SourceError::InvalidSetting {
            key: "settings",
            expected: "only keys this source type uses",
        });
    }
    let mut r = Reader {
        settings,
        strict: true,
        adjusted: Vec::new(),
    };
    let parsed = read(&mut r)?;
    Ok(parsed)
}

/// Reads `settings` as a document is read: a value of the wrong kind takes its default
/// and a value out of range is clamped, and each such key is returned so the host can
/// report `SourceSettingsAdjusted`. Keys of other types are ignored.
pub fn read_text_lenient(settings: &Settings) -> (TextSettings, Vec<&'static str>) {
    let mut r = Reader {
        settings,
        strict: false,
        adjusted: Vec::new(),
    };
    match read(&mut r) {
        Ok(t) => (t, r.adjusted),
        // The lenient reader never fails; keep a usable default if it ever did.
        Err(_) => (
            TextSettings {
                kind: TextKind::Text,
                block: TextBlock::new("", DEFAULT_BOX),
                file: None,
                file_lines: 0,
                fields: false,
                format: None,
                target: None,
                duration_ms: None,
                end_text: None,
                single_line: false,
            },
            r.adjusted,
        ),
    }
}

struct Reader<'a> {
    settings: &'a Settings,
    strict: bool,
    adjusted: Vec<&'static str>,
}

impl Reader<'_> {
    fn bad<T>(
        &mut self,
        key: &'static str,
        expected: &'static str,
        fallback: T,
    ) -> Result<T, SourceError> {
        if self.strict {
            return Err(SourceError::InvalidSetting { key, expected });
        }
        self.adjusted.push(key);
        Ok(fallback)
    }

    fn text(&mut self, key: &'static str) -> Result<Option<String>, SourceError> {
        match self.settings.get(key) {
            None => Ok(None),
            Some(SettingValue::Text(t)) => Ok(Some(t.clone())),
            Some(_) => self.bad(key, "text", None),
        }
    }

    fn flag(&mut self, key: &'static str, default: bool) -> Result<bool, SourceError> {
        match self.settings.get(key) {
            None => Ok(default),
            Some(SettingValue::Bool(b)) => Ok(*b),
            Some(_) => self.bad(key, "a bool", default),
        }
    }

    fn int(
        &mut self,
        key: &'static str,
        (lo, hi): (i64, i64),
        default: i64,
        expected: &'static str,
    ) -> Result<i64, SourceError> {
        match self.settings.get(key) {
            None => Ok(default),
            Some(SettingValue::Int(n)) if (lo..=hi).contains(n) => Ok(*n),
            Some(SettingValue::Int(n)) => {
                let v = (*n).clamp(lo, hi);
                self.bad(key, expected, v)
            }
            Some(_) => self.bad(key, "an integer", default),
        }
    }

    /// A number; an integer is accepted for it.
    fn float(
        &mut self,
        key: &'static str,
        (lo, hi): (f32, f32),
        default: f32,
        expected: &'static str,
    ) -> Result<f32, SourceError> {
        let v = match self.settings.get(key) {
            None => return Ok(default),
            Some(SettingValue::Float(f)) => *f,
            Some(SettingValue::Int(n)) => *n as f64,
            Some(_) => return self.bad(key, "a number", default),
        };
        if v.is_finite() && v >= f64::from(lo) && v <= f64::from(hi) {
            return Ok(v as f32);
        }
        let clamped = if v.is_finite() {
            (v as f32).clamp(lo, hi)
        } else {
            default
        };
        self.bad(key, expected, clamped)
    }

    fn color(&mut self, key: &'static str) -> Result<Option<[u8; 4]>, SourceError> {
        match self.text(key)? {
            None => Ok(None),
            Some(t) => match parse_color(&t) {
                Some(c) => Ok(Some(c)),
                None => self.bad(key, "#rrggbb or #rrggbbaa", None),
            },
        }
    }

    fn choice<T: Copy>(
        &mut self,
        key: &'static str,
        options: &[(&str, T)],
        default: T,
        expected: &'static str,
    ) -> Result<T, SourceError> {
        match self.text(key)? {
            None => Ok(default),
            Some(t) => match options.iter().find(|(name, _)| *name == t) {
                Some((_, v)) => Ok(*v),
                None => self.bad(key, expected, default),
            },
        }
    }
}

fn read(r: &mut Reader<'_>) -> Result<TextSettings, SourceError> {
    let kind = match r.text(keys::KIND)? {
        None => TextKind::Text,
        Some(k) => match TextKind::parse(&k) {
            Some(kind) => kind,
            None => r.bad(
                keys::KIND,
                "text, clock, countdown, stopwatch, recording_time or stream_time",
                TextKind::Text,
            )?,
        },
    };
    let mut text = r.text(keys::TEXT)?.unwrap_or_default();
    if text.chars().count() > MAX_CHARS {
        let cut: String = text.chars().take(MAX_CHARS).collect();
        text = r.bad(keys::TEXT, "at most 10000 characters", cut)?;
    }
    let side = (1, i64::from(MAX_SIDE));
    let w = r.int(
        keys::WIDTH,
        side,
        i64::from(DEFAULT_BOX[0]),
        "1 to 4096 pixels",
    )? as u32;
    let h = r.int(
        keys::HEIGHT,
        side,
        i64::from(DEFAULT_BOX[1]),
        "1 to 4096 pixels",
    )? as u32;
    let given = (
        r.settings.contains_key(keys::WIDTH),
        r.settings.contains_key(keys::HEIGHT),
    );
    let (w, h) = if given == (true, true) {
        (w, h)
    } else {
        r.bad(keys::WIDTH, "a box width and height", (w, h))?
    };
    let (w, h) = if u64::from(w) * u64::from(h) > MAX_PIXELS {
        // Keep the width and shorten the box to the pixel cap.
        let h_max = (MAX_PIXELS / u64::from(w)) as u32;
        r.bad(keys::HEIGHT, "a box of at most 8388608 pixels", (w, h_max))?
    } else {
        (w, h)
    };
    let mut block = TextBlock::new(text, [w, h]);
    block.font = r.text(keys::FONT)?.filter(|f| !f.trim().is_empty());
    block.size = r.float(keys::SIZE, SIZE_RANGE, 72.0, "1 to 1024 pixels")?;
    let weights = (i64::from(WEIGHT_RANGE.0), i64::from(WEIGHT_RANGE.1));
    block.weight = r.int(keys::WEIGHT, weights, 400, "100 to 900")? as u16;
    block.italic = r.flag(keys::ITALIC, false)?;
    let color = r.color(keys::COLOR)?.unwrap_or([255, 255, 255, 255]);
    let color2 = r.color(keys::COLOR2)?;
    let angle = r.int(keys::ANGLE, (-360, 360), 0, "-360 to 360 degrees")?;
    block.fill = Fill {
        color,
        gradient: color2.map(|c| (c, angle as f32)),
    };
    block.align = r.choice(
        keys::ALIGN,
        &[
            ("left", Align::Left),
            ("center", Align::Center),
            ("right", Align::Right),
        ],
        Align::Center,
        "left, center or right",
    )?;
    block.valign = r.choice(
        keys::VALIGN,
        &[
            ("top", VAlign::Top),
            ("middle", VAlign::Middle),
            ("bottom", VAlign::Bottom),
        ],
        VAlign::Middle,
        "top, middle or bottom",
    )?;
    block.wrap = r.flag(keys::WRAP, true)?;
    block.overflow = r.choice(
        keys::OVERFLOW,
        &[
            ("fit", Overflow::Fit),
            ("clip", Overflow::Clip),
            ("ellipsis", Overflow::Ellipsis),
        ],
        Overflow::Fit,
        "fit, clip or ellipsis",
    )?;
    block.line_spacing = r.float(keys::LINE_SPACING, LINE_SPACING_RANGE, 1.0, "0.5 to 3")?;
    block.letter_spacing = r.float(
        keys::LETTER_SPACING,
        LETTER_SPACING_RANGE,
        0.0,
        "-50 to 200 pixels",
    )?;
    let outline_width = r.float(
        keys::OUTLINE_WIDTH,
        (0.0, MAX_STROKE),
        0.0,
        "0 to 64 pixels",
    )?;
    let outline_color = r.color(keys::OUTLINE_COLOR)?.unwrap_or([0, 0, 0, 255]);
    block.outline = (outline_width > 0.0).then_some(Outline {
        width: outline_width,
        color: outline_color,
    });
    let offset = (-MAX_SHADOW_OFFSET, MAX_SHADOW_OFFSET);
    let shadow_color = r.color(keys::SHADOW_COLOR)?;
    let shadow_x = r.float(keys::SHADOW_X, offset, 0.0, "-256 to 256 pixels")?;
    let shadow_y = r.float(keys::SHADOW_Y, offset, 0.0, "-256 to 256 pixels")?;
    let shadow_blur = r.float(keys::SHADOW_BLUR, (0.0, MAX_STROKE), 0.0, "0 to 64 pixels")?;
    block.shadow = shadow_color.map(|color| Shadow {
        color,
        offset: [shadow_x, shadow_y],
        blur: shadow_blur,
    });
    block.background = r.color(keys::BACKGROUND)?;
    block.padding = r.float(keys::PADDING, (0.0, MAX_PADDING), 0.0, "0 to 1024 pixels")?;
    let file = r.text(keys::FILE)?.filter(|f| !f.trim().is_empty());
    let file_lines = r.int(keys::FILE_LINES, (0, i64::from(u32::MAX)), 0, "0 or more")? as u32;
    let fields = r.flag(keys::FIELDS, false)?;
    let format = r.text(keys::FORMAT)?;
    let target = match r.text(keys::TARGET)? {
        Some(t) if crate::text_live::parse_target(&t).is_none() => {
            r.bad(keys::TARGET, "a time of day HH:mm or HH:mm:ss", None)?
        }
        t => t,
    };
    let duration_ms = match r.settings.get(keys::DURATION_MS) {
        None => None,
        Some(_) => Some(r.int(keys::DURATION_MS, (0, i64::MAX), 0, "0 or more")? as u64),
    };
    let end_text = r.text(keys::END_TEXT)?;
    let single_line = r.flag(keys::SINGLE_LINE, false)?;
    block.tabular_figures = kind != TextKind::Text || fields;
    Ok(TextSettings {
        kind,
        block,
        file,
        file_lines,
        fields,
        format,
        target,
        duration_ms,
        end_text,
        single_line,
    })
}

/// The block a `single_line` source draws (ADR-0150 decision 10): every line break
/// (`\r\n`, `\n`, `\r`) is one space, nothing wraps, and the box is as wide as the
/// text measures (`natural_width`, from [`rbs_text::TextRenderer::measure`] of the
/// returned block's text without a width), within a side of [`MAX_SIDE`] and the
/// [`MAX_PIXELS`] area at the block's height. A wider text keeps the widest box and
/// is cut there (the renderer reports `cut_to_fit`). The height stays as set.
pub fn single_line_block(mut block: TextBlock) -> TextBlock {
    block.text = one_line(&block.text);
    block.wrap = false;
    block
}

/// The box width of a `single_line` block whose text measures `natural_width`.
pub fn single_line_width(natural_width: u32, height: u32) -> u32 {
    let by_area = (MAX_PIXELS / u64::from(height.max(1))).min(u64::from(MAX_SIDE)) as u32;
    natural_width.clamp(1, by_area)
}

fn one_line(text: &str) -> String {
    text.replace("\r\n", " ").replace(['\n', '\r'], " ")
}

#[cfg(test)]
#[allow(clippy::unwrap_used)] // Tests fail by panicking.
mod tests {
    use super::*;

    fn settings(pairs: &[(&str, SettingValue)]) -> Settings {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), v.clone()))
            .collect()
    }

    fn boxed(extra: &[(&str, SettingValue)]) -> Settings {
        let mut s = settings(&[
            (keys::WIDTH, SettingValue::Int(400)),
            (keys::HEIGHT, SettingValue::Int(100)),
        ]);
        s.extend(settings(extra));
        s
    }

    #[test]
    fn defaults_follow_the_adr() {
        let t = validate_text(&boxed(&[(keys::TEXT, SettingValue::Text("Hi".into()))])).unwrap();
        assert_eq!(t.kind, TextKind::Text);
        assert_eq!(t.block.box_size, [400, 100]);
        assert_eq!(t.block.size, 72.0);
        assert_eq!(t.block.weight, 400);
        assert_eq!(t.block.align, Align::Center);
        assert_eq!(t.block.valign, VAlign::Middle);
        assert_eq!(t.block.overflow, Overflow::Fit);
        assert!(t.block.wrap);
        assert_eq!(t.block.fill.color, [255, 255, 255, 255]);
        assert!(t.block.outline.is_none() && t.block.shadow.is_none());
        assert!(t.block.validate().is_ok());
        assert!(t.is_still());
        assert!(!t.single_line);
    }

    #[test]
    fn a_single_line_has_no_breaks_and_its_width_stays_within_the_caps() {
        let t = validate_text(&boxed(&[(keys::SINGLE_LINE, SettingValue::Bool(true))])).unwrap();
        assert!(t.single_line);
        assert!(validate_text(&boxed(&[(keys::SINGLE_LINE, SettingValue::Int(1))])).is_err());
        let mut b = t.block;
        b.text = "a\r\nb\nc\rd".into();
        let b = single_line_block(b);
        assert_eq!(b.text, "a b c d");
        assert!(!b.wrap);
        assert_eq!(single_line_width(900, 100), 900);
        assert_eq!(single_line_width(0, 100), 1);
        assert_eq!(single_line_width(9000, 100), MAX_SIDE);
        // At 4096 high the area cap allows 2048 across.
        assert_eq!(single_line_width(9000, 4096), 2048);
    }

    #[test]
    fn every_setting_reaches_the_block() {
        let t = validate_text(&boxed(&[
            (keys::FONT, SettingValue::Text("Inter".into())),
            (keys::SIZE, SettingValue::Float(30.5)),
            (keys::WEIGHT, SettingValue::Int(700)),
            (keys::ITALIC, SettingValue::Bool(true)),
            (keys::COLOR, SettingValue::Text("#ff0000".into())),
            (keys::COLOR2, SettingValue::Text("#0000ff80".into())),
            (keys::ANGLE, SettingValue::Int(90)),
            (keys::ALIGN, SettingValue::Text("left".into())),
            (keys::VALIGN, SettingValue::Text("bottom".into())),
            (keys::WRAP, SettingValue::Bool(false)),
            (keys::OVERFLOW, SettingValue::Text("ellipsis".into())),
            (keys::LINE_SPACING, SettingValue::Float(1.5)),
            (keys::LETTER_SPACING, SettingValue::Int(-3)),
            (keys::OUTLINE_WIDTH, SettingValue::Float(4.0)),
            (keys::OUTLINE_COLOR, SettingValue::Text("#112233".into())),
            (keys::SHADOW_COLOR, SettingValue::Text("#000000".into())),
            (keys::SHADOW_X, SettingValue::Float(2.0)),
            (keys::SHADOW_Y, SettingValue::Float(3.0)),
            (keys::SHADOW_BLUR, SettingValue::Float(5.0)),
            (keys::BACKGROUND, SettingValue::Text("#203040".into())),
            (keys::PADDING, SettingValue::Float(12.0)),
            (keys::FILE, SettingValue::Text("/tmp/x.txt".into())),
            (keys::FILE_LINES, SettingValue::Int(3)),
        ]))
        .unwrap();
        let b = &t.block;
        assert_eq!(b.font.as_deref(), Some("Inter"));
        assert_eq!((b.size, b.weight, b.italic), (30.5, 700, true));
        assert_eq!(b.fill.color, [255, 0, 0, 255]);
        assert_eq!(b.fill.gradient, Some(([0, 0, 255, 128], 90.0)));
        assert_eq!(
            (b.align, b.valign, b.wrap),
            (Align::Left, VAlign::Bottom, false)
        );
        assert_eq!(b.overflow, Overflow::Ellipsis);
        assert_eq!((b.line_spacing, b.letter_spacing), (1.5, -3.0));
        assert_eq!(
            b.outline.map(|o| (o.width, o.color)),
            Some((4.0, [0x11, 0x22, 0x33, 255]))
        );
        let s = b.shadow.unwrap();
        assert_eq!((s.offset, s.blur), ([2.0, 3.0], 5.0));
        assert_eq!(b.background, Some([0x20, 0x30, 0x40, 255]));
        assert_eq!(b.padding, 12.0);
        assert_eq!(t.file.as_deref(), Some("/tmp/x.txt"));
        assert_eq!(t.file_lines, 3);
        assert!(b.validate().is_ok());
    }

    #[test]
    fn edits_out_of_range_are_refused() {
        for (key, value) in [
            (keys::SIZE, SettingValue::Float(0.5)),
            (keys::SIZE, SettingValue::Float(2000.0)),
            (keys::WEIGHT, SettingValue::Int(950)),
            (keys::WIDTH, SettingValue::Int(5000)),
            (keys::OUTLINE_WIDTH, SettingValue::Float(65.0)),
            (keys::SHADOW_X, SettingValue::Float(-300.0)),
            (keys::ALIGN, SettingValue::Text("justify".into())),
            (keys::KIND, SettingValue::Text("weather".into())),
            (keys::COLOR, SettingValue::Text("red".into())),
            (keys::ITALIC, SettingValue::Int(1)),
            (keys::TEXT, SettingValue::Text("x".repeat(MAX_CHARS + 1))),
        ] {
            let err = validate_text(&boxed(&[(key, value)])).unwrap_err();
            assert!(
                matches!(err, SourceError::InvalidSetting { key: k, .. } if k == key),
                "{key}: {err:?}"
            );
        }
        let big = settings(&[
            (keys::WIDTH, SettingValue::Int(4096)),
            (keys::HEIGHT, SettingValue::Int(4096)),
        ]);
        assert!(validate_text(&big).is_err(), "over the pixel cap");
        assert!(validate_text(&Settings::new()).is_err(), "no box");
        assert!(
            validate_text(&boxed(&[(keys::URL, SettingValue::Text("x".into()))])).is_err(),
            "a key of another type"
        );
    }

    #[test]
    fn documents_are_clamped_and_reported() {
        let (t, adjusted) = read_text_lenient(&settings(&[
            (keys::SIZE, SettingValue::Float(5000.0)),
            (keys::WEIGHT, SettingValue::Text("bold".into())),
            (keys::KIND, SettingValue::Text("weather".into())),
            (keys::WIDTH, SettingValue::Int(4096)),
            (keys::HEIGHT, SettingValue::Int(4096)),
            (keys::URL, SettingValue::Text("ignored".into())),
        ]));
        assert_eq!(t.block.size, 1024.0);
        assert_eq!(t.block.weight, 400);
        assert_eq!(t.kind, TextKind::Text);
        assert_eq!(t.block.box_size, [4096, 2048]);
        assert!(t.block.validate().is_ok());
        for key in [keys::SIZE, keys::WEIGHT, keys::KIND, keys::HEIGHT] {
            assert!(adjusted.contains(&key), "{key} not reported: {adjusted:?}");
        }
        let (t, adjusted) = read_text_lenient(&Settings::new());
        assert_eq!(t.block.box_size, DEFAULT_BOX);
        assert_eq!(adjusted, vec![keys::WIDTH]);
    }

    #[test]
    fn the_on_air_set_holds_every_item_of_shown_scenes_hidden_ones_too() {
        use rbs_core::media::Size;
        use rbs_core::timing::FrameRate;
        use rbs_graph::node::NodeTypeId;
        use rbs_scene::SceneCollection;
        use rbs_scene::model::{ItemContent, Layout, SceneScope};
        let mut col = SceneCollection::new();
        let (canvas, _) = col
            .add_canvas("Main", Size::new(1920, 1080), FrameRate::FPS_30)
            .unwrap();
        let text = |col: &mut SceneCollection, name: &str| {
            col.add_source(
                name,
                NodeTypeId::new(crate::types::TEXT_SOURCE),
                Settings::new(),
            )
            .unwrap()
            .0
        };
        let (shown, hidden, nested, held, off, window) = (
            text(&mut col, "a"),
            text(&mut col, "b"),
            text(&mut col, "c"),
            text(&mut col, "d"),
            text(&mut col, "e"),
            text(&mut col, "f"),
        );
        let (color, _) = col
            .add_source(
                "bg",
                NodeTypeId::new(crate::types::COLOR_SOURCE),
                Settings::new(),
            )
            .unwrap();
        let (live, _) = col.add_scene("Live", SceneScope::Shared).unwrap();
        let (inner, _) = col.add_scene("Inner", SceneScope::Shared).unwrap();
        let (hold, _) = col.add_scene("Hold", SceneScope::Shared).unwrap();
        let (other, _) = col.add_scene("Other", SceneScope::Shared).unwrap();
        let full = Layout::new([0.0, 0.0], [100.0, 100.0]);
        col.add_item(live, ItemContent::Source(shown), full, None)
            .unwrap();
        let (item, _) = col
            .add_item(live, ItemContent::Source(hidden), full, None)
            .unwrap();
        col.set_item_visible(item, false).unwrap();
        col.add_item(live, ItemContent::Source(color), full, None)
            .unwrap();
        col.add_item(live, ItemContent::Scene(inner), full, None)
            .unwrap();
        col.add_item(inner, ItemContent::Source(nested), full, None)
            .unwrap();
        col.add_item(hold, ItemContent::Source(held), full, None)
            .unwrap();
        col.add_item(other, ItemContent::Source(off), full, None)
            .unwrap();
        col.set_program(canvas, Some(live)).unwrap();
        let set = text_on_air(&col, [hold], [window, color]);
        assert_eq!(
            set,
            [shown, hidden, nested, held, window].into_iter().collect()
        );
    }

    #[test]
    fn a_text_marked_as_images_goes_back_to_auto_when_it_starts_to_move() {
        use rbs_graph::node::NodeTypeId;
        use rbs_scene::model::{RecordSeparately, VideoAs};
        let text = NodeTypeId::new(crate::types::TEXT_SOURCE);
        let color = NodeTypeId::new(crate::types::COLOR_SOURCE);
        let images = RecordSeparately::new(true, false).with_video_as(VideoAs::Image);
        let clock = settings(&[(keys::KIND, SettingValue::Text("clock".into()))]);
        let fields = settings(&[(keys::FIELDS, SettingValue::Bool(true))]);
        let plain = settings(&[(keys::TEXT, SettingValue::Text("x".into()))]);
        let auto = Some(images.with_video_as(VideoAs::Auto));
        assert_eq!(crate::mark_after_edit(&text, &clock, images), auto);
        assert_eq!(crate::mark_after_edit(&text, &fields, images), auto);
        assert_eq!(crate::mark_after_edit(&text, &plain, images), None);
        assert_eq!(crate::mark_after_edit(&color, &clock, images), None);
        let video = RecordSeparately::new(true, false).with_video_as(VideoAs::Video);
        assert_eq!(crate::mark_after_edit(&text, &clock, video), None);
    }

    #[test]
    fn only_plain_text_without_fields_is_still() {
        assert!(text_is_still(&Settings::new()));
        assert!(text_is_still(&settings(&[(
            keys::KIND,
            SettingValue::Text("text".into())
        )])));
        for s in [
            settings(&[(keys::KIND, SettingValue::Text("clock".into()))]),
            settings(&[(keys::KIND, SettingValue::Text("weather".into()))]),
            settings(&[(keys::FIELDS, SettingValue::Bool(true))]),
        ] {
            assert!(!text_is_still(&s), "{s:?}");
        }
        let clock =
            validate_text(&boxed(&[(keys::KIND, SettingValue::Text("clock".into()))])).unwrap();
        assert!(!clock.is_still());
        assert!(clock.block.tabular_figures);
    }
}
