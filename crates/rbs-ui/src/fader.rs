//! The volume fader (ADR-0059): where a source's volume sits on a slider, and back.
//!
//! The fader uses the meters' scale ([`crate::meter::position`]): linear in dB from
//! [`FLOOR_DB`] at the left end to [`TOP_DB`] (0 dB, full volume) at the right end, so
//! the fader lines up with the meter it sits beside. The left end itself is silence (a
//! volume of 0), not −60 dB. Volumes are linear amplitude factors from 0 to 1, as the
//! scene model keeps them.

use crate::meter::{FLOOR_DB, TOP_DB, position};

/// The volume at fader position `pos` (0 to 1; outside or not a number is clamped):
/// 0 at the left end, else `10^(dB / 20)` with dB linear from [`FLOOR_DB`] to
/// [`TOP_DB`].
pub fn volume_at(pos: f32) -> f32 {
    let pos = if pos.is_nan() {
        0.0
    } else {
        pos.clamp(0.0, 1.0)
    };
    if pos <= 0.0 {
        return 0.0;
    }
    let db = FLOOR_DB + pos * (TOP_DB - FLOOR_DB);
    10f32.powf(db / 20.0).min(1.0)
}

/// The fader position of `volume` (a linear factor): 0 for silence or a volume at or
/// below [`FLOOR_DB`], 1 at full volume and above.
pub fn position_of(volume: f32) -> f32 {
    if volume.is_nan() || volume <= 0.0 {
        return 0.0;
    }
    position(20.0 * volume.log10())
}

/// The volume in words, for the fader's label and screen readers: "0 dB", "−6.0 dB",
/// "Silent".
pub fn volume_text(volume: f32) -> String {
    if volume.is_nan() || volume <= 0.0 {
        return "Silent".to_owned();
    }
    let db = 20.0 * volume.log10();
    if db > -0.05 {
        "0 dB".to_owned()
    } else {
        format!("\u{2212}{:.1} dB", -db)
    }
}

/// The fader position for a level the user typed (ADR-0059): a number of dB such as
/// "-6", "-6 dB", "\u{2212}6,5" or "0", or a word for silence ("-inf", "mute", "silent",
/// "off"). Above 0 dB is full volume and below [`FLOOR_DB`] is silence, because the
/// fader's range is the whole range (boost is a filter, not the fader). `None` for
/// anything else, so the caller keeps the old level.
pub fn parse_position(text: &str) -> Option<f32> {
    let text = text
        .trim()
        .to_lowercase()
        .replace('\u{2212}', "-")
        .replace(',', ".");
    let text = text.strip_suffix("db").unwrap_or(&text).trim();
    if matches!(
        text,
        "-inf" | "inf" | "-infinity" | "mute" | "muted" | "silent" | "off"
    ) {
        return Some(0.0);
    }
    let db: f32 = text.parse().ok()?;
    if !db.is_finite() {
        return None;
    }
    if db >= TOP_DB {
        Some(1.0)
    } else if db < FLOOR_DB {
        Some(0.0)
    } else {
        Some(position_of(10f32.powf(db / 20.0)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ends_are_silence_and_full_volume() {
        assert_eq!(volume_at(0.0), 0.0);
        assert_eq!(volume_at(-1.0), 0.0);
        assert_eq!(volume_at(f32::NAN), 0.0);
        assert!((volume_at(1.0) - 1.0).abs() < 1e-6);
        assert!((volume_at(2.0) - 1.0).abs() < 1e-6);
        assert_eq!(position_of(0.0), 0.0);
        assert_eq!(position_of(f32::NAN), 0.0);
        assert!((position_of(1.0) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn the_fader_is_linear_in_db_like_the_meter() {
        // Half way is -30 dB.
        let v = volume_at(0.5);
        assert!((20.0 * v.log10() + 30.0).abs() < 1e-3, "{v}");
        // -6 dB sits where the meter shows -6 dB.
        let half = 10f32.powf(-6.0 / 20.0);
        assert!((position_of(half) - position(-6.0)).abs() < 1e-5);
        // Round trips across the fader.
        for i in 1..=100 {
            let pos = i as f32 / 100.0;
            assert!((position_of(volume_at(pos)) - pos).abs() < 1e-4, "{pos}");
        }
    }

    #[test]
    fn volumes_read_in_db() {
        assert_eq!(volume_text(1.0), "0 dB");
        assert_eq!(volume_text(0.0), "Silent");
        assert_eq!(volume_text(10f32.powf(-6.0 / 20.0)), "\u{2212}6.0 dB");
    }

    #[test]
    fn typed_levels_become_fader_positions() {
        let at = |t: &str| parse_position(t).unwrap_or(f32::NAN);
        assert_eq!(at("0"), 1.0);
        assert_eq!(at("+3"), 1.0);
        assert_eq!(at(" 0 dB "), 1.0);
        assert_eq!(at("-inf"), 0.0);
        assert_eq!(at("Mute"), 0.0);
        assert_eq!(at("-80"), 0.0);
        assert!((at("-6") - position(-6.0)).abs() < 1e-5);
        assert!((at("-6 dB") - at("-6")).abs() < 1e-6);
        assert!((at("\u{2212}6,5 DB") - position(-6.5)).abs() < 1e-5);
        // What the label says reads back as the same level.
        let label = volume_text(volume_at(0.7));
        assert!((at(&label) - 0.7).abs() < 0.01, "{label}");
        for bad in ["", "dB", "abc", "-", "--6", "nan", "6 6"] {
            assert_eq!(parse_position(bad), None, "{bad:?}");
        }
    }
}
