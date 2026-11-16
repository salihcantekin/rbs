//! OBS item transforms to RBS layouts (ADR-0131 decision 8).
//!
//! OBS draws a source pixel `p` (after the item crop) at
//! `pos + R(rot) · (local(p) − origin)`, where `local` scales the picture (or fits it
//! into the bounds box) and `origin` is the alignment anchor of that box. RBS stores the
//! box rotated about its centre (`Layout`). [`place`] computes the four corners of the
//! OBS picture in texture order and [`layout_from_corners`] returns the RBS box whose
//! texture lands on the same corners, so a flip stays a flip and never becomes a turn.

use rbs_scene::model::{Crop, Layout};

/// OBS alignment bits.
pub const ALIGN_LEFT: u32 = 1;
/// OBS alignment bits.
pub const ALIGN_RIGHT: u32 = 2;
/// OBS alignment bits.
pub const ALIGN_TOP: u32 = 4;
/// OBS alignment bits.
pub const ALIGN_BOTTOM: u32 = 8;

/// How an OBS item is fitted into its bounds box (`bounds_type`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bounds {
    /// 0: no bounds box; the picture is native size times `scale`.
    None,
    /// 1: stretched to the box.
    Stretch,
    /// 2: as large as fits inside the box.
    ScaleInner,
    /// 3: as small as covers the box.
    ScaleOuter,
    /// 4: as wide as the box.
    ToWidth,
    /// 5: as tall as the box.
    ToHeight,
    /// 6: native size, shrunk to fit when larger than the box.
    MaxOnly,
}

impl Bounds {
    /// The type for an OBS `bounds_type`; an unknown value is `None`.
    pub fn from_obs(v: u64) -> Self {
        match v {
            1 => Self::Stretch,
            2 => Self::ScaleInner,
            3 => Self::ScaleOuter,
            4 => Self::ToWidth,
            5 => Self::ToHeight,
            6 => Self::MaxOnly,
            _ => Self::None,
        }
    }
}

/// An OBS item transform as stored in a scene's `items`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ObsTransform {
    /// `pos`, canvas pixels.
    pub pos: [f64; 2],
    /// `rot`, degrees clockwise.
    pub rot: f64,
    /// `scale`; negative flips.
    pub scale: [f64; 2],
    /// `align` bits.
    pub align: u32,
    /// `bounds_type`.
    pub bounds: Bounds,
    /// `bounds_align` bits.
    pub bounds_align: u32,
    /// `bounds`, canvas pixels.
    pub bounds_size: [f64; 2],
    /// `bounds_crop` (OBS 30.1+): the picture is clipped to the bounds box.
    pub bounds_crop: bool,
    /// `crop_left`, `crop_top`, `crop_right`, `crop_bottom`, source pixels.
    pub crop: [u32; 4],
}

impl Default for ObsTransform {
    fn default() -> Self {
        Self {
            pos: [0.0, 0.0],
            rot: 0.0,
            scale: [1.0, 1.0],
            align: ALIGN_LEFT | ALIGN_TOP,
            bounds: Bounds::None,
            bounds_align: 0,
            bounds_size: [0.0, 0.0],
            bounds_crop: false,
            crop: [0; 4],
        }
    }
}

/// Where an item's picture lands: its corners in texture order (top-left, top-right,
/// bottom-left, bottom-right of the cropped picture) and the crop that picture has.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Placed {
    /// Canvas points of the picture's corners, in texture order.
    pub corners: [[f64; 2]; 4],
    /// Crop in source pixels, including what `bounds_crop` cut.
    pub crop: [u32; 4],
    /// Rotation that the corners are expected to carry, degrees clockwise.
    pub rot: f64,
}

fn align_offset(align: u32, size: [f64; 2]) -> [f64; 2] {
    let x = if align & ALIGN_LEFT != 0 {
        0.0
    } else if align & ALIGN_RIGHT != 0 {
        size[0]
    } else {
        size[0] / 2.0
    };
    let y = if align & ALIGN_TOP != 0 {
        0.0
    } else if align & ALIGN_BOTTOM != 0 {
        size[1]
    } else {
        size[1] / 2.0
    };
    [x, y]
}

fn rotate(v: [f64; 2], deg: f64) -> [f64; 2] {
    let (s, c) = deg.to_radians().sin_cos();
    [v[0] * c - v[1] * s, v[0] * s + v[1] * c]
}

/// The OBS picture of a source of `native` size placed by `t`. `None` when the cropped
/// picture or the bounds box is empty.
pub fn place(t: &ObsTransform, native: (u32, u32)) -> Option<Placed> {
    let [cl, ct, cr, cb] = t.crop;
    let cx = f64::from(native.0.saturating_sub(cl.saturating_add(cr)));
    let cy = f64::from(native.1.saturating_sub(ct.saturating_add(cb)));
    if cx <= 0.0 || cy <= 0.0 {
        return None;
    }
    let flip = [t.scale[0] < 0.0, t.scale[1] < 0.0];
    // Picture rectangle in local space (before the anchor and rotation) and the
    // absolute scale from source pixels to local pixels.
    let (pic_origin, abs_scale, anchor_box) = if t.bounds == Bounds::None {
        let s = [t.scale[0].abs(), t.scale[1].abs()];
        let size = [cx * s[0], cy * s[1]];
        ([0.0, 0.0], s, size)
    } else {
        let [bx, by] = t.bounds_size;
        if bx <= 0.0 || by <= 0.0 {
            return None;
        }
        let (sx, sy) = (bx / cx, by / cy);
        let s = match t.bounds {
            Bounds::Stretch => [sx, sy],
            Bounds::ScaleInner => [sx.min(sy); 2],
            Bounds::ScaleOuter => [sx.max(sy); 2],
            Bounds::ToWidth => [sx; 2],
            Bounds::ToHeight => [sy; 2],
            Bounds::MaxOnly if cx > bx || cy > by => [sx.min(sy); 2],
            Bounds::MaxOnly | Bounds::None => [1.0, 1.0],
        };
        let size = [cx * s[0], cy * s[1]];
        let inner = align_offset(t.bounds_align, [bx - size[0], by - size[1]]);
        (inner, s, [bx, by])
    };
    let origin = align_offset(t.align, anchor_box);
    let size = [cx * abs_scale[0], cy * abs_scale[1]];
    let mut crop = t.crop;
    // The visible picture rectangle in local space.
    let mut lo = pic_origin;
    let mut hi = [pic_origin[0] + size[0], pic_origin[1] + size[1]];
    if t.bounds != Bounds::None && t.bounds_crop {
        let bx = anchor_box;
        let cut = |lo: f64, hi: f64, max: f64| ((-lo).max(0.0), (hi - max).max(0.0));
        let (x0, x1) = cut(lo[0], hi[0], bx[0]);
        let (y0, y1) = cut(lo[1], hi[1], bx[1]);
        // Local cuts to source pixels; a flipped axis cuts the other source edge.
        let px = |v: f64, axis: usize| (v / abs_scale[axis]).round() as u32;
        let (left, right) = if flip[0] { (x1, x0) } else { (x0, x1) };
        let (top, bottom) = if flip[1] { (y1, y0) } else { (y0, y1) };
        crop[0] = crop[0].saturating_add(px(left, 0));
        crop[2] = crop[2].saturating_add(px(right, 0));
        crop[1] = crop[1].saturating_add(px(top, 1));
        crop[3] = crop[3].saturating_add(px(bottom, 1));
        lo = [lo[0] + x0, lo[1] + y0];
        hi = [hi[0] - x1, hi[1] - y1];
        if hi[0] <= lo[0] || hi[1] <= lo[1] {
            return None;
        }
    }
    // Texture u=0 is on the left of the visible rectangle unless that axis is flipped.
    let xs = if flip[0] {
        [hi[0], lo[0]]
    } else {
        [lo[0], hi[0]]
    };
    let ys = if flip[1] {
        [hi[1], lo[1]]
    } else {
        [lo[1], hi[1]]
    };
    let screen = |x: f64, y: f64| {
        let r = rotate([x - origin[0], y - origin[1]], t.rot);
        [r[0] + t.pos[0], r[1] + t.pos[1]]
    };
    Some(Placed {
        corners: [
            screen(xs[0], ys[0]),
            screen(xs[1], ys[0]),
            screen(xs[0], ys[1]),
            screen(xs[1], ys[1]),
        ],
        crop,
        rot: t.rot,
    })
}

/// The canvas point of `p` in a group's own pixel space of `size`, for a group whose
/// picture was placed at `group`.
pub fn through_group(group: &Placed, size: (u32, u32), p: [f64; 2]) -> [f64; 2] {
    let [g00, g10, g01, _] = group.corners;
    let u = p[0] / f64::from(size.0.max(1));
    let v = p[1] / f64::from(size.1.max(1));
    [
        g00[0] + u * (g10[0] - g00[0]) + v * (g01[0] - g00[0]),
        g00[1] + u * (g10[1] - g00[1]) + v * (g01[1] - g00[1]),
    ]
}

/// A child placed inside a group, moved onto the canvas.
pub fn compose(group: &Placed, group_size: (u32, u32), child: &Placed) -> Placed {
    let mut corners = child.corners;
    for c in &mut corners {
        *c = through_group(group, group_size, *c);
    }
    Placed {
        corners,
        crop: child.crop,
        rot: group.rot + child.rot,
    }
}

/// The RBS layout whose texture covers `placed`'s corners, and whether it is only an
/// approximation (the corners are not a rotated rectangle: a sheared group).
pub fn layout_from_corners(placed: &Placed) -> (Layout, bool) {
    let [q00, q10, q01, q11] = placed.corners;
    let centre = [(q00[0] + q11[0]) / 2.0, (q00[1] + q11[1]) / 2.0];
    let eu = [q10[0] - q00[0], q10[1] - q00[1]];
    let ev = [q01[0] - q00[0], q01[1] - q00[1]];
    let (s, c) = placed.rot.to_radians().sin_cos();
    let sx = eu[0] * c + eu[1] * s;
    let sy = -ev[0] * s + ev[1] * c;
    // Residuals: the part of each edge the rotated box cannot express.
    let ru = (eu[0] - sx * c).hypot(eu[1] - sx * s);
    let rv = (ev[0] + sy * s).hypot(ev[1] - sy * c);
    let tol = 0.01_f64.max(1e-6 * (sx.abs() + sy.abs()));
    let sheared = ru > tol || rv > tol;
    let position = [centre[0] - sx / 2.0, centre[1] - sy / 2.0];
    let mut rot = placed.rot % 360.0;
    if rot > 180.0 {
        rot -= 360.0;
    } else if rot <= -180.0 {
        rot += 360.0;
    }
    let [l, t, r, b] = placed.crop;
    let layout = Layout::new(
        [position[0] as f32, position[1] as f32],
        [sx as f32, sy as f32],
    )
    .with_rotation(rot as f32)
    .with_crop(Crop::new(l, t, r, b));
    (layout, sheared)
}

/// Corners in texture order of an RBS layout, the inverse of
/// [`layout_from_corners`], for tests.
pub fn corners_of(layout: &Layout) -> [[f64; 2]; 4] {
    let p = [f64::from(layout.position[0]), f64::from(layout.position[1])];
    let s = [f64::from(layout.size[0]), f64::from(layout.size[1])];
    let centre = [p[0] + s[0] / 2.0, p[1] + s[1] / 2.0];
    let rot = f64::from(layout.rotation);
    let at = |u: f64, v: f64| {
        let local = [p[0] + u * s[0] - centre[0], p[1] + v * s[1] - centre[1]];
        let r = rotate(local, rot);
        [r[0] + centre[0], r[1] + centre[1]]
    };
    [at(0.0, 0.0), at(1.0, 0.0), at(0.0, 1.0), at(1.0, 1.0)]
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.
mod tests {
    use super::*;

    fn close(a: [[f64; 2]; 4], b: [[f64; 2]; 4], tol: f64) -> bool {
        a.iter()
            .zip(b.iter())
            .all(|(p, q)| (p[0] - q[0]).abs() <= tol && (p[1] - q[1]).abs() <= tol)
    }

    #[test]
    fn a_plain_item_is_its_native_box() {
        let t = ObsTransform {
            pos: [100.0, 50.0],
            ..ObsTransform::default()
        };
        let p = place(&t, (640, 360)).unwrap();
        let (l, approx) = layout_from_corners(&p);
        assert!(!approx);
        assert_eq!(l.position, [100.0, 50.0]);
        assert_eq!(l.size, [640.0, 360.0]);
        assert_eq!(l.rotation, 0.0);
    }

    #[test]
    fn centre_alignment_and_rotation_keep_the_centre() {
        // OBS rotates about `pos`; with centre alignment that is the picture's centre.
        let t = ObsTransform {
            pos: [960.0, 540.0],
            rot: 90.0,
            align: 0,
            scale: [0.5, 0.5],
            ..ObsTransform::default()
        };
        let p = place(&t, (1920, 1080)).unwrap();
        let (l, _) = layout_from_corners(&p);
        assert_eq!(l.size, [960.0, 540.0]);
        assert_eq!(l.position, [480.0, 270.0]);
        assert_eq!(l.rotation, 90.0);
    }

    #[test]
    fn rotation_about_a_corner_moves_the_box() {
        // Top-left alignment, 90° clockwise about the top-left corner: the picture
        // hangs to the left of `pos`.
        let t = ObsTransform {
            pos: [500.0, 100.0],
            rot: 90.0,
            ..ObsTransform::default()
        };
        let p = place(&t, (200, 100)).unwrap();
        // Texture top-left stays at pos; top-right goes down.
        assert!(close(
            p.corners,
            [
                [500.0, 100.0],
                [500.0, 300.0],
                [400.0, 100.0],
                [400.0, 300.0]
            ],
            1e-9
        ));
        let (l, _) = layout_from_corners(&p);
        assert!(close(corners_of(&l), p.corners, 0.01));
    }

    #[test]
    fn a_flip_is_a_negative_size_not_a_turn() {
        let t = ObsTransform {
            pos: [960.0, 540.0],
            align: 0,
            scale: [-1.0, 1.0],
            ..ObsTransform::default()
        };
        let p = place(&t, (400, 200)).unwrap();
        let (l, _) = layout_from_corners(&p);
        assert_eq!(l.rotation, 0.0);
        assert!(l.size[0] < 0.0 && l.size[1] > 0.0, "{:?}", l.size);
        assert!(close(corners_of(&l), p.corners, 0.01));
    }

    #[test]
    fn scale_inner_bounds_centre_the_picture() {
        let t = ObsTransform {
            bounds: Bounds::ScaleInner,
            bounds_size: [1920.0, 1080.0],
            bounds_align: 0,
            ..ObsTransform::default()
        };
        // A 4:3 picture fits 1440 x 1080, pillarboxed.
        let p = place(&t, (640, 480)).unwrap();
        let (l, _) = layout_from_corners(&p);
        assert_eq!(l.position, [240.0, 0.0]);
        assert_eq!(l.size, [1440.0, 1080.0]);
    }

    #[test]
    fn bounds_crop_cuts_the_overflow_in_source_pixels() {
        let t = ObsTransform {
            bounds: Bounds::ScaleOuter,
            bounds_size: [1080.0, 1080.0],
            bounds_align: 0,
            bounds_crop: true,
            ..ObsTransform::default()
        };
        // 1920 x 1080 covers 1080 x 1080 at scale 1: 420 px cut on each side.
        let p = place(&t, (1920, 1080)).unwrap();
        assert_eq!(p.crop, [420, 0, 420, 0]);
        let (l, _) = layout_from_corners(&p);
        assert_eq!(l.position, [0.0, 0.0]);
        assert_eq!(l.size, [1080.0, 1080.0]);
    }

    #[test]
    fn crop_shrinks_the_picture_before_scaling() {
        let t = ObsTransform {
            scale: [2.0, 2.0],
            crop: [10, 20, 30, 40],
            ..ObsTransform::default()
        };
        let p = place(&t, (100, 100)).unwrap();
        let (l, _) = layout_from_corners(&p);
        assert_eq!(l.size, [120.0, 80.0]);
        assert_eq!(l.crop, Crop::new(10, 20, 30, 40));
    }

    #[test]
    fn an_empty_picture_places_nothing() {
        let t = ObsTransform {
            crop: [60, 0, 60, 0],
            ..ObsTransform::default()
        };
        assert!(place(&t, (100, 100)).is_none());
    }

    #[test]
    fn a_group_moves_and_scales_its_children() {
        let group = place(
            &ObsTransform {
                pos: [100.0, 100.0],
                scale: [0.5, 0.5],
                ..ObsTransform::default()
            },
            (800, 600),
        )
        .unwrap();
        let child = place(
            &ObsTransform {
                pos: [200.0, 100.0],
                ..ObsTransform::default()
            },
            (400, 200),
        )
        .unwrap();
        let (l, approx) = layout_from_corners(&compose(&group, (800, 600), &child));
        assert!(!approx);
        assert_eq!(l.position, [200.0, 150.0]);
        assert_eq!(l.size, [200.0, 100.0]);
    }

    #[test]
    fn a_sheared_group_is_approximate() {
        let group = place(
            &ObsTransform {
                scale: [2.0, 1.0],
                ..ObsTransform::default()
            },
            (100, 100),
        )
        .unwrap();
        let child = place(
            &ObsTransform {
                pos: [50.0, 50.0],
                rot: 45.0,
                align: 0,
                ..ObsTransform::default()
            },
            (20, 20),
        )
        .unwrap();
        let (_, approx) = layout_from_corners(&compose(&group, (100, 100), &child));
        assert!(approx);
    }

    /// Property: for any transform, the RBS box draws the texture on the OBS corners
    /// within 0.01 px (ADR-0131 decision 13). Deterministic: a fixed-seed generator.
    #[test]
    fn corners_agree_for_random_transforms() {
        let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state >> 11) as f64 / (1u64 << 53) as f64
        };
        for _ in 0..5000 {
            let mut range = |lo: f64, hi: f64| lo + next() * (hi - lo);
            let bounds = Bounds::from_obs(range(0.0, 7.0) as u64);
            let mut scale = [range(0.1, 3.0), range(0.1, 3.0)];
            if range(0.0, 1.0) < 0.3 {
                scale[0] = -scale[0];
            }
            if range(0.0, 1.0) < 0.3 {
                scale[1] = -scale[1];
            }
            let t = ObsTransform {
                pos: [range(-2000.0, 4000.0), range(-2000.0, 4000.0)],
                rot: range(-720.0, 720.0),
                scale,
                align: range(0.0, 16.0) as u32,
                bounds,
                bounds_align: range(0.0, 16.0) as u32,
                bounds_size: [range(10.0, 3000.0), range(10.0, 3000.0)],
                bounds_crop: false,
                crop: [range(0.0, 50.0) as u32, range(0.0, 50.0) as u32, 0, 0],
            };
            let native = (range(100.0, 4000.0) as u32, range(100.0, 4000.0) as u32);
            let Some(p) = place(&t, native) else { continue };
            let (l, approx) = layout_from_corners(&p);
            assert!(!approx, "{t:?}");
            // f32 storage of canvas coordinates up to a few thousand pixels.
            assert!(close(corners_of(&l), p.corners, 0.01), "{t:?} {native:?}");
        }
    }
}
