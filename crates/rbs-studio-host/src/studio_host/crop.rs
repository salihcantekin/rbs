//! Crop regions, transform steps and the crop refusals.

use super::*;

/// A part of a source's picture the Region menu shows (ADR-0058).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CropRegion {
    /// The whole picture: no crop, fitted to the canvas like a new source.
    Full,
    /// The centre, as large as the canvas (or the picture, if smaller), shown 1:1 when
    /// the picture is at least as large as the canvas.
    CanvasCentred,
    /// The left half.
    LeftHalf,
    /// The right half.
    RightHalf,
}

impl CropRegion {
    /// The crop that shows this part of a `w x h` picture on a `cw x ch` canvas.
    pub fn crop(self, (w, h): (u32, u32), (cw, ch): (u32, u32)) -> Crop {
        match self {
            CropRegion::Full => Crop::default(),
            CropRegion::CanvasCentred => {
                let (sw, sh) = (cw.min(w), ch.min(h));
                let (l, t) = ((w - sw) / 2, (h - sh) / 2);
                Crop::new(l, t, w - sw - l, h - sh - t)
            }
            CropRegion::LeftHalf => Crop::new(0, 0, w - w / 2, 0),
            CropRegion::RightHalf => Crop::new(w / 2, 0, 0, 0),
        }
    }
}

/// What a transform command does to an item's layout (ADR-0084).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TransformOp {
    /// Mirror left to right as seen on the canvas.
    FlipHorizontal,
    /// Mirror top to bottom as seen on the canvas.
    FlipVertical,
    /// Turn clockwise by this many degrees (negative turns the other way).
    Turn(f32),
    /// Set the rotation to this many degrees clockwise, wrapped into `0..360`.
    SetAngle(f32),
    /// Place on the canvas (needs the picture size).
    Fit(FitMode),
    /// Move to the canvas centre on these axes.
    Center(CenterAxis),
    /// Not turned, not flipped, native size (needs the picture size).
    Reset,
}

/// A picture this small on both sides is only a fill the item's box stretches, so its
/// pixel size is no size to reset to.
const FILL_ONLY: u32 = 2;

/// The source's `box_width` x `box_height`, when it stores both as positive numbers.
/// Reset transform needs no picture then.
pub(super) fn stored_box(settings: &Settings) -> Option<(u32, u32)> {
    let side = |key: &str| match settings.get(key) {
        Some(SettingValue::Int(n)) => u32::try_from(*n).ok().filter(|n| *n > 0),
        _ => None,
    };
    Some((side(keys::BOX_WIDTH)?, side(keys::BOX_HEIGHT)?))
}

/// The size Reset transform gives an item whose visible picture is `shown` (ADR-0084
/// decision 4): the source's `box_width` x `box_height` when it stores them (a Library
/// shape or plain frame), else the picture. A fill-only picture without them (a shape
/// saved before the keys existed) keeps the item's `current` size.
pub(super) fn reset_size(settings: &Settings, shown: (u32, u32), current: [f32; 2]) -> (u32, u32) {
    if let Some(size) = stored_box(settings) {
        return size;
    }
    if shown.0 <= FILL_ONLY && shown.1 <= FILL_ONLY {
        let keep = |v: f32| (v.abs().round() as u32).max(1);
        return (keep(current[0]), keep(current[1]));
    }
    shown
}

/// Why a crop was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum CropRefusal {
    /// No such item, or its source is gone.
    NotFound(String),
    /// The request cannot be met (a nested scene, a crop that leaves nothing).
    Invalid(String),
    /// The scale cannot be kept: the source never showed a picture.
    NoPicture,
    /// The scene refused the edit.
    Failed(String),
    /// The canvas cannot be edited now (the vertical canvas shows another scene, or
    /// the source is global there).
    Conflict(String),
}

impl CropRefusal {
    pub(super) fn text(&self) -> String {
        match self {
            CropRefusal::NotFound(t)
            | CropRefusal::Invalid(t)
            | CropRefusal::Failed(t)
            | CropRefusal::Conflict(t) => t.clone(),
            CropRefusal::NoPicture => {
                "This source has no picture yet, so its crop cannot keep the scale.".into()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_box(w: i64, h: i64) -> Settings {
        let mut s = Settings::new();
        s.insert(keys::BOX_WIDTH.into(), SettingValue::Int(w));
        s.insert(keys::BOX_HEIGHT.into(), SettingValue::Int(h));
        s
    }

    #[test]
    fn a_shape_resets_to_the_box_it_was_added_with() {
        let size = reset_size(&with_box(484, 302), (2, 2), [20.0, 20.0]);
        assert_eq!(size, (484, 302));
    }

    #[test]
    fn a_fill_only_picture_without_a_box_keeps_its_size() {
        let size = reset_size(&Settings::new(), (2, 2), [-300.4, 120.0]);
        assert_eq!(size, (300, 120));
    }

    #[test]
    fn a_picture_resets_to_its_size_and_a_bad_box_is_ignored() {
        let picture = (1920, 1080);
        assert_eq!(reset_size(&Settings::new(), picture, [10.0, 10.0]), picture);
        assert_eq!(
            reset_size(&with_box(0, 300), picture, [10.0, 10.0]),
            picture
        );
    }
}
