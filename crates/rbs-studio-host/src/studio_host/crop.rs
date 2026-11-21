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
