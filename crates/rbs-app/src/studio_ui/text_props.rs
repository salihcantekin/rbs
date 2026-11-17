//! The Text tab of a text source in Properties (ADR-0129 decision 13): what it shows,
//! read from the source's stored settings, and the setting patches its edits send.
//! Pure functions; the studio sends each patch as one undo step.

use std::collections::BTreeMap;

use rbs_scene::model::{SettingValue, Settings};
use rbs_sources::text::{TextKind, read_text_lenient};
use rbs_sources::types::keys;
use rbs_text::{Align, Rgba};
use serde_json::Value;

use crate::ui::TextData;

/// The kinds in the Kind list's order.
pub const KINDS: [TextKind; 6] = [
    TextKind::Text,
    TextKind::Clock,
    TextKind::Countdown,
    TextKind::Stopwatch,
    TextKind::RecordingTime,
    TextKind::StreamTime,
];

/// What turning an effect on starts from, when the source has none stored.
const OUTLINE_WIDTH: f64 = 4.0;
const SHADOW: &str = "#000000b3";
const SHADOW_OFFSET: f64 = 4.0;
const SHADOW_BLUR: f64 = 4.0;
const BACKGROUND: &str = "#000000b3";
const BACKGROUND_PADDING: f64 = 16.0;
/// A new countdown counts five minutes.
const COUNTDOWN_MS: i64 = 5 * 60 * 1000;

/// A patch of setting keys: a `null` removes a key.
pub type Patch = BTreeMap<String, Value>;

fn hex(c: Rgba) -> String {
    let [r, g, b, a] = c;
    if a == 255 {
        format!("#{r:02x}{g:02x}{b:02x}")
    } else {
        format!("#{r:02x}{g:02x}{b:02x}{a:02x}")
    }
}

fn swatch(text: &str) -> slint::Color {
    let [r, g, b, a] = rbs_api::types::parse_color(text).unwrap_or([0, 0, 0, 255]);
    slint::Color::from_argb_u8(a, r, g, b)
}

fn text_of(settings: &Settings, key: &str) -> Option<String> {
    match settings.get(key) {
        Some(SettingValue::Text(t)) => Some(t.clone()),
        _ => None,
    }
}

fn number_of(settings: &Settings, key: &str) -> Option<f64> {
    match settings.get(key) {
        Some(SettingValue::Int(i)) => Some(*i as f64),
        Some(SettingValue::Float(f)) => Some(*f),
        _ => None,
    }
}

/// The file's name without its folder, for the Read from file row.
fn file_name(path: &str) -> String {
    std::path::Path::new(path)
        .file_name()
        .map_or_else(|| path.to_owned(), |n| n.to_string_lossy().into_owned())
}

/// The Text tab's data for the text source `id` with `settings`. Values that are
/// absent or out of range read as the source draws them; an effect that is off keeps
/// showing its stored or starting values.
pub fn text_data(id: i32, settings: &Settings) -> TextData {
    let (parsed, _) = read_text_lenient(settings);
    let b = &parsed.block;
    let color = hex(b.fill.color);
    let outline_color = b.outline.map_or_else(
        || text_of(settings, keys::OUTLINE_COLOR).unwrap_or_else(|| "#000000".into()),
        |o| hex(o.color),
    );
    let shadow_color = b.shadow.map_or_else(|| SHADOW.to_owned(), |s| hex(s.color));
    let background_color = b.background.map_or_else(|| BACKGROUND.to_owned(), hex);
    let duration = parsed.duration_ms.unwrap_or(0) / 1000;
    let file = parsed.file.clone().unwrap_or_default();
    TextData {
        on: true,
        id,
        kind: KINDS
            .iter()
            .position(|k| *k == parsed.kind)
            .map_or(0, |i| i as i32),
        text: b.text.as_str().into(),
        file_name: file_name(&file).into(),
        file: file.into(),
        file_lines: parsed.file_lines as i32,
        fields: parsed.fields,
        font: b.font.clone().unwrap_or_default().into(),
        size: b.size,
        bold: b.weight >= 600,
        italic: b.italic,
        swatch: swatch(&color),
        color: color.into(),
        align: match b.align {
            Align::Left => 0,
            Align::Right => 2,
            _ => 1,
        },
        format: parsed.format.clone().unwrap_or_default().into(),
        format_default: match parsed.kind {
            TextKind::Clock => "HH:mm:ss",
            _ if duration >= 3600 => "H:mm:ss",
            _ => "mm:ss",
        }
        .into(),
        until: parsed.target.is_some(),
        target: parsed.target.clone().unwrap_or_default().into(),
        minutes: (duration / 60).min(i32::MAX as u64) as i32,
        seconds: (duration % 60) as i32,
        end_text: parsed.end_text.clone().unwrap_or_default().into(),
        outline: b.outline.is_some(),
        outline_width: b.outline.map_or(OUTLINE_WIDTH as f32, |o| o.width),
        outline_swatch: swatch(&outline_color),
        outline_color: outline_color.into(),
        shadow: b.shadow.is_some(),
        shadow_swatch: swatch(&shadow_color),
        shadow_color: shadow_color.into(),
        shadow_x: b.shadow.map_or(SHADOW_OFFSET as f32, |s| s.offset[0]),
        shadow_y: b.shadow.map_or(SHADOW_OFFSET as f32, |s| s.offset[1]),
        shadow_blur: b.shadow.map_or(SHADOW_BLUR as f32, |s| s.blur),
        background: b.background.is_some(),
        background_swatch: swatch(&background_color),
        background_color: background_color.into(),
        padding: if b.background.is_some() || b.padding > 0.0 {
            b.padding
        } else {
            BACKGROUND_PADDING as f32
        },
    }
}

fn number(v: f64) -> Value {
    if v.fract() == 0.0 && v.abs() < 1e15 {
        Value::from(v as i64)
    } else {
        serde_json::Number::from_f64(v).map_or(Value::Null, Value::Number)
    }
}

/// One key: text, or removed when `value` is empty (an empty `text` stays text).
pub fn value_patch(key: &str, value: &str) -> Patch {
    let v = if value.is_empty() && key != keys::TEXT {
        Value::Null
    } else {
        Value::String(value.to_owned())
    };
    BTreeMap::from([(key.to_owned(), v)])
}

/// One number key.
pub fn number_patch(key: &str, value: f64) -> Patch {
    BTreeMap::from([(key.to_owned(), number(value))])
}

/// One flag key.
pub fn flag_patch(key: &str, value: bool) -> Patch {
    BTreeMap::from([(key.to_owned(), Value::Bool(value))])
}

/// Kind `index` of [`KINDS`]. A countdown without a length or a time counts five
/// minutes; another kind's pattern is dropped, since it rarely fits.
pub fn kind_patch(settings: &Settings, index: i32) -> Option<Patch> {
    let kind = *KINDS.get(usize::try_from(index).ok()?)?;
    let mut p = BTreeMap::from([(keys::KIND.to_owned(), Value::from(kind.as_str()))]);
    if settings.contains_key(keys::FORMAT) {
        p.insert(keys::FORMAT.into(), Value::Null);
    }
    if kind == TextKind::Countdown
        && !settings.contains_key(keys::TARGET)
        && number_of(settings, keys::DURATION_MS).unwrap_or(0.0) <= 0.0
    {
        p.insert(keys::DURATION_MS.into(), Value::from(COUNTDOWN_MS));
    }
    Some(p)
}

/// Turns an effect on or off: 0 outline, 1 shadow, 2 background. On starts from the
/// stored values, or a dark, readable look when there are none.
pub fn effect_patch(settings: &Settings, effect: i32, on: bool) -> Patch {
    let mut p = Patch::new();
    let mut start = |key: &str, v: Value| {
        if !settings.contains_key(key) {
            p.insert(key.to_owned(), v);
        }
    };
    match (effect, on) {
        (0, true) => {
            if number_of(settings, keys::OUTLINE_WIDTH).unwrap_or(0.0) <= 0.0 {
                p.insert(keys::OUTLINE_WIDTH.into(), number(OUTLINE_WIDTH));
            }
        }
        (0, false) => {
            p.insert(keys::OUTLINE_WIDTH.into(), Value::from(0));
        }
        (1, true) => {
            start(keys::SHADOW_COLOR, Value::from(SHADOW));
            start(keys::SHADOW_X, number(SHADOW_OFFSET));
            start(keys::SHADOW_Y, number(SHADOW_OFFSET));
            start(keys::SHADOW_BLUR, number(SHADOW_BLUR));
        }
        (1, false) => {
            p.insert(keys::SHADOW_COLOR.into(), Value::Null);
        }
        (2, true) => {
            start(keys::BACKGROUND, Value::from(BACKGROUND));
            if number_of(settings, keys::PADDING).unwrap_or(0.0) <= 0.0 {
                p.insert(keys::PADDING.into(), number(BACKGROUND_PADDING));
            }
        }
        (2, false) => {
            p.insert(keys::BACKGROUND.into(), Value::Null);
        }
        _ => {}
    }
    p
}

/// A countdown until `target` (a time of day) or for `seconds`.
pub fn countdown_patch(until: bool, target: &str, seconds: i32) -> Patch {
    let target = target.trim();
    if until {
        BTreeMap::from([(
            keys::TARGET.to_owned(),
            Value::from(if target.is_empty() { "00:00" } else { target }),
        )])
    } else {
        BTreeMap::from([
            (keys::TARGET.to_owned(), Value::Null),
            (
                keys::DURATION_MS.to_owned(),
                Value::from(i64::from(seconds.max(0)) * 1000),
            ),
        ])
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)] // Tests fail by panicking.
mod tests {
    use super::*;

    fn settings(pairs: &[(&str, SettingValue)]) -> Settings {
        let mut s = Settings::new();
        s.insert(keys::WIDTH.into(), SettingValue::Int(640));
        s.insert(keys::HEIGHT.into(), SettingValue::Int(160));
        for (k, v) in pairs {
            s.insert((*k).into(), v.clone());
        }
        s
    }

    fn applied(mut s: Settings, p: &Patch) -> Settings {
        for (k, v) in p {
            match v {
                Value::Null => {
                    s.remove(k);
                }
                Value::Bool(b) => {
                    s.insert(k.clone(), SettingValue::Bool(*b));
                }
                Value::Number(n) => {
                    let v = n
                        .as_i64()
                        .map_or(SettingValue::Float(n.as_f64().unwrap()), SettingValue::Int);
                    s.insert(k.clone(), v);
                }
                Value::String(t) => {
                    s.insert(k.clone(), SettingValue::Text(t.clone()));
                }
                _ => unreachable!(),
            }
        }
        s
    }

    #[test]
    fn a_new_text_reads_as_the_source_draws_it() {
        let d = text_data(3, &settings(&[("text", SettingValue::Text("Hi".into()))]));
        assert_eq!(
            (d.id, d.kind, d.text.as_str(), d.font.as_str(), d.size),
            (3, 0, "Hi", "", 72.0)
        );
        assert_eq!((d.color.as_str(), d.align, d.bold), ("#ffffff", 1, false));
        assert!(!d.outline && !d.shadow && !d.background);
        // An effect that is off shows what turning it on starts from.
        assert_eq!(d.shadow_color.as_str(), SHADOW);
        assert_eq!(d.padding, 16.0);
        assert_eq!(d.format_default.as_str(), "mm:ss");
    }

    #[test]
    fn each_kind_patch_validates_and_a_countdown_counts_five_minutes() {
        let base = settings(&[("text", SettingValue::Text("Hi".into()))]);
        for i in 0..KINDS.len() as i32 {
            let s = applied(base.clone(), &kind_patch(&base, i).unwrap());
            rbs_sources::text::validate_text(&s).unwrap();
            assert_eq!(text_data(1, &s).kind, i);
        }
        let s = applied(base.clone(), &kind_patch(&base, 2).unwrap());
        let d = text_data(1, &s);
        assert_eq!((d.minutes, d.seconds, d.until), (5, 0, false));
        assert!(kind_patch(&base, 6).is_none());
    }

    #[test]
    fn effects_turn_on_with_a_readable_start_and_off_again() {
        let base = settings(&[("text", SettingValue::Text("Hi".into()))]);
        let mut s = base.clone();
        for effect in 0..3 {
            s = applied(s.clone(), &effect_patch(&s, effect, true));
        }
        rbs_sources::text::validate_text(&s).unwrap();
        let d = text_data(1, &s);
        assert!(d.outline && d.shadow && d.background);
        assert_eq!(
            (d.outline_width, d.shadow_blur, d.padding),
            (4.0, 4.0, 16.0)
        );
        for effect in 0..3 {
            s = applied(s.clone(), &effect_patch(&s, effect, false));
        }
        rbs_sources::text::validate_text(&s).unwrap();
        let d = text_data(1, &s);
        assert!(!d.outline && !d.shadow && !d.background);
    }

    #[test]
    fn a_countdown_switches_between_a_length_and_a_time() {
        let base = settings(&[("kind", SettingValue::Text("countdown".into()))]);
        let s = applied(base.clone(), &countdown_patch(true, " 18:30 ", 0));
        rbs_sources::text::validate_text(&s).unwrap();
        let d = text_data(1, &s);
        assert_eq!((d.until, d.target.as_str()), (true, "18:30"));
        let s = applied(s, &countdown_patch(false, "18:30", 90));
        let d = text_data(1, &s);
        assert_eq!((d.until, d.minutes, d.seconds), (false, 1, 30));
    }

    #[test]
    fn an_empty_value_removes_its_key_but_text_stays() {
        assert_eq!(value_patch("file", "")["file"], Value::Null);
        assert_eq!(value_patch("text", "")["text"], Value::from(""));
        assert_eq!(number_patch("size", 48.0)["size"], Value::from(48));
        assert_eq!(number_patch("size", 48.5)["size"], Value::from(48.5));
    }
}
