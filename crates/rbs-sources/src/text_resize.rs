//! Crisp resizing of a text source (ADR-0129 decision 12, spec `scene-sources.md`,
//! "Crisp resizing"): a new item size on the canvas becomes a new box and scaled
//! settings, so the text is drawn again at the size it is shown instead of stretched.
//! Pure: the host decides when a resize may be crisp.

use rbs_scene::model::{Crop, SettingValue, Settings};
use rbs_text::{
    LETTER_SPACING_RANGE, MAX_PADDING, MAX_PIXELS, MAX_SHADOW_OFFSET, MAX_SIDE, MAX_STROKE,
    SIZE_RANGE,
};

use crate::text::DEFAULT_BOX;
use crate::types::keys;

/// Factors closer than this count as the aspect ratio kept.
const SAME_FACTOR: f64 = 0.01;

/// A crisp resize: the text's new settings and crop, and the size the item gets on
/// the canvas (one picture pixel per canvas pixel unless a cap stopped the box).
#[derive(Debug, Clone, PartialEq)]
pub struct Resized {
    /// The settings with the new box and scaled sizes.
    pub settings: Settings,
    /// The crop scaled per axis.
    pub crop: Crop,
    /// The item's new size on the canvas, positive: the new box less the crop, or
    /// `size` as asked when a cap stopped the box (the rest is stretched).
    pub size: [f32; 2],
}

fn number(settings: &Settings, key: &str, default: f64) -> f64 {
    match settings.get(key) {
        Some(SettingValue::Float(v)) => *v,
        Some(SettingValue::Int(v)) => *v as f64,
        _ => default,
    }
}

/// The resize of a text whose item shows `crop` of its box and is now `size` canvas
/// pixels (absolute values). `None` when the box would not change (a cap reached
/// already, or the same size) or the picture shows nothing, and for a `single_line`
/// text, whose picture is as wide as its text, not as its box (it stretches for now).
pub fn resized(settings: &Settings, crop: Crop, size: [f32; 2]) -> Option<Resized> {
    if matches!(
        settings.get(keys::SINGLE_LINE),
        Some(SettingValue::Bool(true))
    ) {
        return None;
    }
    let w = number(settings, keys::WIDTH, f64::from(DEFAULT_BOX[0])).max(1.0);
    let h = number(settings, keys::HEIGHT, f64::from(DEFAULT_BOX[1])).max(1.0);
    let vis_w = w - f64::from(crop.left) - f64::from(crop.right);
    let vis_h = h - f64::from(crop.top) - f64::from(crop.bottom);
    let (want_w, want_h) = (f64::from(size[0].abs()), f64::from(size[1].abs()));
    if vis_w < 1.0 || vis_h < 1.0 || !want_w.is_finite() || !want_h.is_finite() {
        return None;
    }
    let (mut fx, mut fy) = (want_w / vis_w, want_h / vis_h);
    if fx <= 0.0 || fy <= 0.0 {
        return None;
    }
    let kept = (fx - fy).abs() <= SAME_FACTOR * fx.max(fy);
    let text_factor = |fx: f64, fy: f64| if kept { fx.min(fy) } else { fy };
    // Caps: a side, the pixel count, the font size. Past one, the box stops and the
    // rest is stretched.
    let font = number(settings, keys::SIZE, 72.0);
    let side = f64::from(MAX_SIDE);
    let mut cap = (side / (w * fx)).min(side / (h * fy));
    cap = cap.min((MAX_PIXELS as f64 / (w * fx * h * fy)).sqrt());
    cap = cap.min(f64::from(SIZE_RANGE.1) / (font * text_factor(fx, fy)));
    let capped = cap < 1.0;
    if capped {
        fx *= cap;
        fy *= cap;
    }
    let box_w = (w * fx).round().clamp(1.0, side);
    let box_h = (h * fy).round().clamp(1.0, side);
    if box_w == w && box_h == h {
        return None;
    }
    let scale = |v: u32, f: f64| (f64::from(v) * f).round() as u32;
    let crop = Crop::new(
        scale(crop.left, fx),
        scale(crop.top, fy),
        scale(crop.right, fx),
        scale(crop.bottom, fy),
    );
    if f64::from(crop.left + crop.right) >= box_w || f64::from(crop.top + crop.bottom) >= box_h {
        return None;
    }
    let f = text_factor(fx, fy);
    let mut out = settings.clone();
    out.insert(keys::WIDTH.into(), SettingValue::Int(box_w as i64));
    out.insert(keys::HEIGHT.into(), SettingValue::Int(box_h as i64));
    let mut put = |key: &str, default: f64, lo: f32, hi: f32| {
        if key == keys::SIZE || settings.contains_key(key) {
            let v = (number(settings, key, default) * f).clamp(f64::from(lo), f64::from(hi));
            // Two decimals: settings stay short and equal sizes compare equal.
            out.insert(key.into(), SettingValue::Float((v * 100.0).round() / 100.0));
        }
    };
    put(keys::SIZE, 72.0, SIZE_RANGE.0, SIZE_RANGE.1);
    put(keys::OUTLINE_WIDTH, 0.0, 0.0, MAX_STROKE);
    put(keys::SHADOW_X, 0.0, -MAX_SHADOW_OFFSET, MAX_SHADOW_OFFSET);
    put(keys::SHADOW_Y, 0.0, -MAX_SHADOW_OFFSET, MAX_SHADOW_OFFSET);
    put(keys::SHADOW_BLUR, 0.0, 0.0, MAX_STROKE);
    put(keys::PADDING, 0.0, 0.0, MAX_PADDING);
    put(
        keys::LETTER_SPACING,
        0.0,
        LETTER_SPACING_RANGE.0,
        LETTER_SPACING_RANGE.1,
    );
    let size = if capped {
        [size[0].abs(), size[1].abs()]
    } else {
        [
            (box_w - f64::from(crop.left + crop.right)) as f32,
            (box_h - f64::from(crop.top + crop.bottom)) as f32,
        ]
    };
    Some(Resized {
        settings: out,
        crop,
        size,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)] // Tests fail by panicking.
mod tests {
    use super::*;

    fn text(w: i64, h: i64, extra: &[(&str, SettingValue)]) -> Settings {
        let mut s = Settings::new();
        s.insert(keys::WIDTH.into(), SettingValue::Int(w));
        s.insert(keys::HEIGHT.into(), SettingValue::Int(h));
        s.insert(keys::SIZE.into(), SettingValue::Float(40.0));
        for (k, v) in extra {
            s.insert((*k).into(), v.clone());
        }
        s
    }

    fn f(s: &Settings, key: &str) -> f64 {
        number(s, key, -1.0)
    }

    #[test]
    fn a_single_line_stretches() {
        let s = text(200, 50, &[(keys::SINGLE_LINE, SettingValue::Bool(true))]);
        assert_eq!(resized(&s, Crop::default(), [400.0, 100.0]), None);
    }

    #[test]
    fn doubling_doubles_the_box_and_every_size() {
        let s = text(
            200,
            50,
            &[
                (keys::OUTLINE_WIDTH, SettingValue::Float(2.0)),
                (keys::SHADOW_X, SettingValue::Float(-3.0)),
                (keys::PADDING, SettingValue::Int(4)),
                (keys::LETTER_SPACING, SettingValue::Float(1.5)),
            ],
        );
        let r = resized(&s, Crop::default(), [400.0, 100.0]).unwrap();
        assert_eq!(r.settings[keys::WIDTH], SettingValue::Int(400));
        assert_eq!(r.settings[keys::HEIGHT], SettingValue::Int(100));
        assert_eq!(f(&r.settings, keys::SIZE), 80.0);
        assert_eq!(f(&r.settings, keys::OUTLINE_WIDTH), 4.0);
        assert_eq!(f(&r.settings, keys::SHADOW_X), -6.0);
        assert_eq!(f(&r.settings, keys::PADDING), 8.0);
        assert_eq!(f(&r.settings, keys::LETTER_SPACING), 3.0);
        assert!(
            !r.settings.contains_key(keys::SHADOW_BLUR),
            "absent keys stay absent"
        );
        assert_eq!(r.size, [400.0, 100.0]);
    }

    #[test]
    fn a_wider_box_keeps_the_font_size_of_the_height() {
        let s = text(200, 50, &[]);
        let r = resized(&s, Crop::default(), [300.0, 50.0]).unwrap();
        assert_eq!(r.settings[keys::WIDTH], SettingValue::Int(300));
        assert_eq!(f(&r.settings, keys::SIZE), 40.0);
        // A taller one scales the text by the height.
        let r = resized(&s, Crop::default(), [200.0, 75.0]).unwrap();
        assert_eq!(f(&r.settings, keys::SIZE), 60.0);
    }

    #[test]
    fn the_crop_scales_per_axis_and_the_item_draws_one_to_one() {
        let s = text(200, 100, &[]);
        let crop = Crop::new(10, 0, 10, 20);
        // Visible 180 x 80, shown at 361 x 160.5: rounded to whole pixels.
        let r = resized(&s, crop, [361.0, -160.5]).unwrap();
        assert_eq!(r.settings[keys::WIDTH], SettingValue::Int(401));
        assert_eq!(r.settings[keys::HEIGHT], SettingValue::Int(201));
        assert_eq!(r.crop, Crop::new(20, 0, 20, 40));
        assert_eq!(r.size, [361.0, 161.0]);
    }

    #[test]
    fn caps_stop_the_box_and_the_rest_is_stretched() {
        let s = text(2000, 100, &[]);
        let r = resized(&s, Crop::default(), [8000.0, 400.0]).unwrap();
        assert_eq!(r.settings[keys::WIDTH], SettingValue::Int(4096));
        assert_eq!(r.settings[keys::HEIGHT], SettingValue::Int(205));
        assert_eq!(r.size, [8000.0, 400.0]);
        // The font size cap stops it too.
        let big = text(200, 100, &[(keys::SIZE, SettingValue::Float(600.0))]);
        let r = resized(&big, Crop::default(), [800.0, 400.0]).unwrap();
        assert_eq!(f(&r.settings, keys::SIZE), 1024.0);
        // At a cap already, nothing changes.
        let at = text(4096, 100, &[]);
        assert_eq!(resized(&at, Crop::default(), [8192.0, 200.0]), None);
    }

    #[test]
    fn the_same_size_or_nothing_shown_changes_nothing() {
        let s = text(200, 50, &[]);
        assert_eq!(resized(&s, Crop::default(), [200.2, 49.9]), None);
        let all = Crop::new(100, 0, 100, 0);
        assert_eq!(resized(&s, all, [10.0, 10.0]), None);
        assert_eq!(resized(&s, Crop::default(), [f32::NAN, 10.0]), None);
    }
}
