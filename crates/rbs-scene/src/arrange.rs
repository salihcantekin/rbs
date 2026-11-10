//! Flip, rotate, fit and align an item's [`Layout`] (ADR-0084).
//!
//! Every function is pure and returns a new layout; nothing here touches the model or
//! the render path. A flip mirrors the item as the user sees it on the canvas, whatever
//! its rotation, and the result is in the *normal form*: the height is never negative
//! (a vertical flip is a negative width with the rotation turned half a circle) and the
//! rotation is in `0..360`.

use crate::model::{Crop, Layout};

/// How [`Layout::fitted`] places a picture on the canvas.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FitMode {
    /// The largest centred box that fits the canvas with the picture's aspect.
    Fit,
    /// The smallest centred box that covers the canvas with the picture's aspect.
    Fill,
    /// The canvas size, the aspect not kept.
    Stretch,
}

/// Which axes [`Layout::centred`] moves the item on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CenterAxis {
    /// Both.
    Both,
    /// Left to right only.
    Horizontal,
    /// Top to bottom only.
    Vertical,
}

/// The flip state of a layout as the user sees it on the canvas.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FlipState {
    /// Mirrored left to right (and not turned).
    pub horizontal: bool,
    /// Mirrored top to bottom (and not turned).
    pub vertical: bool,
}

const EPS: f32 = 1e-3;

fn wrap_degrees(d: f32) -> f32 {
    let r = d.rem_euclid(360.0);
    // `rem_euclid` can return 360.0 for tiny negative inputs.
    if r >= 360.0 { 0.0 } else { r }
}

fn near(a: f32, b: f32) -> bool {
    (a - b).abs() < EPS
}

impl Layout {
    /// The centre of the box, whatever the signs of `size`.
    pub fn centre(&self) -> [f32; 2] {
        [
            self.position[0] + self.size[0] / 2.0,
            self.position[1] + self.size[1] / 2.0,
        ]
    }

    /// The same box (same size and signs) with its centre at `centre`.
    fn with_centre(mut self, centre: [f32; 2]) -> Self {
        self.position = [
            centre[0] - self.size[0] / 2.0,
            centre[1] - self.size[1] / 2.0,
        ];
        self
    }

    /// The same drawing in the normal form: height not negative, rotation in `0..360`,
    /// the centre kept.
    pub fn normalised(&self) -> Self {
        let centre = self.centre();
        let mut out = *self;
        if out.size[1] < 0.0 {
            out.size = [-out.size[0], -out.size[1]];
            out.rotation += 180.0;
        }
        out.rotation = wrap_degrees(out.rotation);
        out.with_centre(centre)
    }

    /// True if the picture is drawn mirrored (one flip, or an odd number of them).
    pub fn is_mirrored(&self) -> bool {
        self.size[0] * self.size[1] < 0.0
    }

    /// The flip state for Properties and menu check marks. A mirrored item that is
    /// turned by something other than a half circle shows neither flip.
    pub fn flip_state(&self) -> FlipState {
        let n = self.normalised();
        let mirrored = n.is_mirrored();
        FlipState {
            horizontal: mirrored && near(n.rotation, 0.0),
            vertical: mirrored && near(n.rotation, 180.0),
        }
    }

    /// Mirrored left to right as seen on the canvas; the centre stays.
    pub fn flipped_horizontally(&self) -> Self {
        let centre = self.centre();
        let mut out = *self;
        out.size[0] = -out.size[0];
        out.rotation = -out.rotation;
        out.with_centre(centre).normalised()
    }

    /// Mirrored top to bottom as seen on the canvas; the centre stays.
    pub fn flipped_vertically(&self) -> Self {
        let centre = self.centre();
        let mut out = *self;
        out.size[1] = -out.size[1];
        out.rotation = -out.rotation;
        out.with_centre(centre).normalised()
    }

    /// Turned clockwise by `degrees` more about the centre; the box stays.
    pub fn turned(&self, degrees: f32) -> Self {
        let mut out = *self;
        out.rotation = wrap_degrees(out.rotation + degrees);
        out
    }

    /// Not turned.
    pub fn unturned(&self) -> Self {
        let mut out = *self;
        out.rotation = 0.0;
        out
    }

    /// The size of the box's bounding rectangle on the canvas for a picture of
    /// `picture` pixels scaled by `scale`, given the current rotation.
    fn bounds_of(picture: (f32, f32), rotation: f32, scale: f32) -> (f32, f32) {
        let (s, c) = rotation.to_radians().sin_cos();
        let (w, h) = (picture.0 * scale, picture.1 * scale);
        (w * c.abs() + h * s.abs(), w * s.abs() + h * c.abs())
    }

    /// Placed on a `canvas` (width, height) according to `mode`, centred, with the
    /// rotation and flips kept. `picture` is the size of the cropped picture. `None`
    /// for an empty picture or canvas.
    pub fn fitted(&self, canvas: (u32, u32), picture: (u32, u32), mode: FitMode) -> Option<Self> {
        if canvas.0 == 0 || canvas.1 == 0 || picture.0 == 0 || picture.1 == 0 {
            return None;
        }
        let n = self.normalised();
        let (cw, ch) = (canvas.0 as f32, canvas.1 as f32);
        let sign_w = if n.size[0] < 0.0 { -1.0 } else { 1.0 };
        let size = match mode {
            FitMode::Stretch => {
                let quarter = near(n.rotation, 90.0) || near(n.rotation, 270.0);
                let (w, h) = if quarter { (ch, cw) } else { (cw, ch) };
                [w * sign_w, h]
            }
            FitMode::Fit | FitMode::Fill => {
                let pic = (picture.0 as f32, picture.1 as f32);
                let (bw, bh) = Self::bounds_of(pic, n.rotation, 1.0);
                let (sx, sy) = (cw / bw, ch / bh);
                let scale = if mode == FitMode::Fit {
                    sx.min(sy)
                } else {
                    sx.max(sy)
                };
                [pic.0 * scale * sign_w, pic.1 * scale]
            }
        };
        let mut out = n;
        out.size = size;
        let out = out.with_centre([cw / 2.0, ch / 2.0]);
        out.is_valid().then_some(out)
    }

    /// Moved so its centre is the canvas centre on the chosen axes.
    pub fn centred(&self, canvas: (u32, u32), axis: CenterAxis) -> Self {
        let mut centre = self.centre();
        let target = [canvas.0 as f32 / 2.0, canvas.1 as f32 / 2.0];
        if axis != CenterAxis::Vertical {
            centre[0] = target[0];
        }
        if axis != CenterAxis::Horizontal {
            centre[1] = target[1];
        }
        self.with_centre(centre)
    }

    /// Not turned, not flipped, at the native size of the cropped `picture`, the
    /// centre kept. Crop, style and opacity stay. `None` for an empty picture.
    pub fn transform_reset(&self, picture: (u32, u32)) -> Option<Self> {
        if picture.0 == 0 || picture.1 == 0 {
            return None;
        }
        let centre = self.centre();
        let mut out = *self;
        out.size = [picture.0 as f32, picture.1 as f32];
        out.rotation = 0.0;
        Some(out.with_centre(centre))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)] // Tests fail by panicking.
mod tests {
    use super::*;

    fn lay() -> Layout {
        Layout::new([100.0, 50.0], [400.0, 200.0])
    }

    fn close(a: [f32; 2], b: [f32; 2]) -> bool {
        near(a[0], b[0]) && near(a[1], b[1])
    }

    #[test]
    fn flipping_twice_restores_every_layout() {
        for r in [0.0, 30.0, 90.0, 180.0, 275.0] {
            let l = lay().with_rotation(r);
            let h = l.flipped_horizontally().flipped_horizontally();
            let v = l.flipped_vertically().flipped_vertically();
            for twice in [h, v] {
                assert!(close(twice.centre(), l.centre()));
                assert!(close(twice.size, l.size), "{r}: {:?}", twice.size);
                assert!(near(twice.rotation, r), "{r}: {}", twice.rotation);
            }
        }
    }

    #[test]
    fn a_flip_keeps_the_centre_and_the_normal_form() {
        let l = lay().with_rotation(30.0);
        for f in [l.flipped_horizontally(), l.flipped_vertically()] {
            assert!(close(f.centre(), l.centre()));
            assert!(f.size[1] >= 0.0);
            assert!((0.0..360.0).contains(&f.rotation));
            assert!(f.is_mirrored());
        }
    }

    #[test]
    fn flip_states_agree_with_the_commands() {
        let l = lay();
        assert_eq!(
            l.flip_state(),
            FlipState {
                horizontal: false,
                vertical: false
            }
        );
        assert!(l.flipped_horizontally().flip_state().horizontal);
        assert!(l.flipped_vertically().flip_state().vertical);
        assert!(!l.flipped_vertically().flip_state().horizontal);
        let both = l.flipped_horizontally().flipped_vertically();
        assert!(!both.is_mirrored());
    }

    #[test]
    fn a_vertical_flip_equals_the_negated_height_drawing() {
        // Negative height and half a turn more draw the same as the negative width.
        let raw = Layout::new([100.0, 50.0], [400.0, -200.0]);
        let n = raw.normalised();
        assert!(n.size[1] > 0.0 && n.size[0] < 0.0);
        assert!(near(n.rotation, 180.0));
        assert!(close(n.centre(), raw.centre()));
    }

    #[test]
    fn turning_wraps_and_keeps_the_box() {
        let l = lay().with_rotation(300.0).turned(90.0);
        assert!(near(l.rotation, 30.0));
        assert_eq!(l.size, [400.0, 200.0]);
        assert!(near(lay().turned(-90.0).rotation, 270.0));
        assert!(near(lay().turned(180.0).unturned().rotation, 0.0));
    }

    #[test]
    fn fit_and_fill_follow_the_picture_aspect() {
        let l = lay();
        let fit = l.fitted((1920, 1080), (1000, 1000), FitMode::Fit).unwrap();
        assert!(close(fit.size, [1080.0, 1080.0]));
        assert!(close(fit.centre(), [960.0, 540.0]));
        let fill = l.fitted((1920, 1080), (1000, 1000), FitMode::Fill).unwrap();
        assert!(close(fill.size, [1920.0, 1920.0]));
        let stretch = l
            .fitted((1920, 1080), (1000, 1000), FitMode::Stretch)
            .unwrap();
        assert!(close(stretch.size, [1920.0, 1080.0]));
    }

    #[test]
    fn a_quarter_turn_fits_the_rotated_bounds() {
        let l = lay().with_rotation(90.0);
        let fit = l.fitted((1920, 1080), (1920, 1080), FitMode::Fit).unwrap();
        // Turned 90 degrees a 16:9 picture is 9:16 on the canvas: the height limits.
        assert!(close(fit.size, [1080.0, 607.5]));
    }

    #[test]
    fn fitting_keeps_a_flip() {
        let l = lay().flipped_horizontally();
        let fit = l.fitted((1920, 1080), (400, 200), FitMode::Fit).unwrap();
        assert!(fit.size[0] < 0.0 && fit.size[1] > 0.0);
    }

    #[test]
    fn an_empty_picture_or_canvas_cannot_be_fitted() {
        assert!(lay().fitted((1920, 1080), (0, 10), FitMode::Fit).is_none());
        assert!(lay().fitted((0, 1080), (10, 10), FitMode::Fit).is_none());
        assert!(lay().transform_reset((0, 0)).is_none());
    }

    #[test]
    fn centring_moves_only_the_chosen_axes() {
        let l = lay();
        let both = l.centred((1920, 1080), CenterAxis::Both);
        assert!(close(both.centre(), [960.0, 540.0]));
        let h = l.centred((1920, 1080), CenterAxis::Horizontal);
        assert!(near(h.centre()[0], 960.0) && near(h.centre()[1], l.centre()[1]));
        let v = l.centred((1920, 1080), CenterAxis::Vertical);
        assert!(near(v.centre()[1], 540.0) && near(v.centre()[0], l.centre()[0]));
    }

    #[test]
    fn reset_transform_keeps_crop_style_and_centre() {
        let l = lay()
            .with_rotation(33.0)
            .flipped_horizontally()
            .with_opacity(0.5);
        let r = l.transform_reset((320, 240)).unwrap();
        assert_eq!(r.size, [320.0, 240.0]);
        assert!(near(r.rotation, 0.0));
        assert!(close(r.centre(), l.centre()));
        assert_eq!(r.opacity, 0.5);
        assert_eq!(r.crop, l.crop);
    }
}

/// How [`auto_layout`] arranges a scene on a canvas of another shape (ADR-0093
/// decision 4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutoLayout {
    /// Each item scaled to fit within the canvas, letterboxed, centred.
    Fit,
    /// Each item scaled to cover the canvas, cut at the sides or the top and bottom
    /// around its focus point.
    Fill,
    /// The visible items in equal bands from top to bottom in layer order (the lowest
    /// layer on top), each covering its band around its focus point.
    Stack,
}

/// One item for [`auto_layout`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AutoItem {
    /// The layout it has now; opacity, rotation-free style and the like are kept.
    pub layout: Layout,
    /// The size of its uncropped picture in pixels.
    pub picture: (u32, u32),
    /// Where the picture is cut around, `0.0` (left or top) to `1.0`; `0.5` is the
    /// centre.
    pub focus: f32,
}

/// Layouts of `items` (the visible ones, in layer order) for a `canvas` of
/// `(width, height)`. The result has one entry per item; `None` where the picture or
/// the canvas is empty (keep the layout). Rotation and flips of the old layout are
/// dropped, as the new box is made for the picture as it is.
pub fn auto_layout(
    mode: AutoLayout,
    canvas: (u32, u32),
    items: &[AutoItem],
) -> Vec<Option<Layout>> {
    let (cw, ch) = (canvas.0 as f32, canvas.1 as f32);
    let n = items.len().max(1) as f32;
    items
        .iter()
        .enumerate()
        .map(|(i, item)| {
            if canvas.0 == 0 || canvas.1 == 0 || item.picture.0 == 0 || item.picture.1 == 0 {
                return None;
            }
            let (pw, ph) = (item.picture.0 as f32, item.picture.1 as f32);
            let mut layout = item.layout;
            layout.rotation = 0.0;
            layout.size = [layout.size[0].abs(), layout.size[1].abs()];
            let out = match mode {
                AutoLayout::Fit => {
                    let scale = (cw / pw).min(ch / ph);
                    let (w, h) = (pw * scale, ph * scale);
                    layout.crop = Crop::new(0, 0, 0, 0);
                    layout.size = [w, h];
                    layout.position = [(cw - w) / 2.0, (ch - h) / 2.0];
                    layout
                }
                AutoLayout::Fill => cover(layout, (pw, ph), (0.0, 0.0, cw, ch), item.focus),
                AutoLayout::Stack => {
                    let band = ch / n;
                    cover(
                        layout,
                        (pw, ph),
                        (0.0, i as f32 * band, cw, band),
                        item.focus,
                    )
                }
            };
            out.is_valid().then_some(out)
        })
        .collect()
}

/// `layout` made to cover the rectangle `(x, y, w, h)` with a picture of `picture`
/// pixels: the box is the rectangle and the picture is cut (crop in source pixels) to
/// its aspect around `focus`.
fn cover(
    mut layout: Layout,
    picture: (f32, f32),
    rect: (f32, f32, f32, f32),
    focus: f32,
) -> Layout {
    let (pw, ph) = picture;
    let (x, y, w, h) = rect;
    let focus = if focus.is_finite() {
        focus.clamp(0.0, 1.0)
    } else {
        0.5
    };
    // The part of the picture that has the rectangle's aspect, as large as possible.
    let (vis_w, vis_h) = if pw * h >= ph * w {
        (ph * w / h, ph)
    } else {
        (pw, pw * h / w)
    };
    let cut_x = ((pw - vis_w) * focus).round().max(0.0);
    let cut_y = ((ph - vis_h) * focus).round().max(0.0);
    let (left, top) = (cut_x as u32, cut_y as u32);
    let right = (pw - vis_w - cut_x).round().max(0.0) as u32;
    let bottom = (ph - vis_h - cut_y).round().max(0.0) as u32;
    layout.crop = Crop::new(left, top, right, bottom);
    layout.size = [w, h];
    layout.position = [x, y];
    layout
}
