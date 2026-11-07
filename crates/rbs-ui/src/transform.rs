//! Moving and resizing a source's box directly on the preview (UI principles: "Sources
//! are positioned, resized, rotated and cropped directly on the preview with handles,
//! snapping guides and keyboard nudging").
//!
//! Toolkit-neutral and pure: the shell passes in where the picture is, the pointer and
//! the box; this module says which handle the pointer is on, what a drag makes of the
//! box, and which rectangles draw the selection. Coordinates are canvas pixels for
//! boxes and logical pixels of the window for the view. Spec:
//! `docs/specs/ui-design-system.md`, "Program and Preview".

use crate::layout::Rect;

/// Side of a handle square on the preview, logical pixels.
pub const HANDLE_SIZE: f32 = 8.0;
/// Width of the selection outline, logical pixels.
pub const OUTLINE_WIDTH: f32 = 1.0;
/// How far from a handle's centre a press still takes it, logical pixels.
pub const HANDLE_REACH: f32 = 8.0;
/// How close an edge or centre must come to a guide to snap to it, logical pixels.
pub const SNAP_DISTANCE: f32 = 8.0;
/// The smallest box a drag makes, canvas pixels.
pub const MIN_SIZE: f32 = 8.0;

/// A box on the canvas: top-left corner and size in canvas pixels. A negative size
/// flips the picture (scene model); the visible area is [`SourceBox::visual`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SourceBox {
    /// Top-left corner of the unrotated box.
    pub position: [f32; 2],
    /// Width and height; negative flips.
    pub size: [f32; 2],
}

impl SourceBox {
    /// A box at `position` of `size`.
    pub const fn new(position: [f32; 2], size: [f32; 2]) -> Self {
        Self { position, size }
    }

    /// The area it covers, with a positive size, in canvas pixels.
    pub fn visual(&self) -> Rect {
        let (x0, x1) = span(self.position[0], self.size[0]);
        let (y0, y1) = span(self.position[1], self.size[1]);
        Rect {
            x: x0,
            y: y0,
            w: x1 - x0,
            h: y1 - y0,
        }
    }

    /// `visual` placed back into a box that flips like this one.
    fn with_visual(&self, r: Rect) -> SourceBox {
        let fx = self.size[0] < 0.0;
        let fy = self.size[1] < 0.0;
        SourceBox {
            position: [
                if fx { r.x + r.w } else { r.x },
                if fy { r.y + r.h } else { r.y },
            ],
            size: [if fx { -r.w } else { r.w }, if fy { -r.h } else { r.h }],
        }
    }

    /// Moved by `dx`, `dy` canvas pixels (keyboard nudging).
    pub fn nudged(&self, dx: f32, dy: f32) -> SourceBox {
        SourceBox {
            position: [self.position[0] + dx, self.position[1] + dy],
            size: self.size,
        }
    }
}

fn span(start: f32, len: f32) -> (f32, f32) {
    if len < 0.0 {
        (start + len, start)
    } else {
        (start, start + len)
    }
}

/// Where the canvas is drawn in the window: the panel's content rectangle and the
/// canvas size. The picture is letterboxed into the panel, centred, like the preview
/// layer places it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Viewport {
    /// The panel's content, logical pixels of the window.
    pub area: Rect,
    /// Canvas width and height in pixels.
    pub canvas: [f32; 2],
}

impl Viewport {
    /// Logical pixels per canvas pixel; 0 for an empty area or canvas.
    pub fn scale(&self) -> f32 {
        let [cw, ch] = self.canvas;
        if !(cw > 0.0 && ch > 0.0 && self.area.w > 0.0 && self.area.h > 0.0) {
            return 0.0;
        }
        (self.area.w / cw).min(self.area.h / ch)
    }

    /// The picture's rectangle in the window.
    pub fn picture(&self) -> Rect {
        let s = self.scale();
        let w = self.canvas[0] * s;
        let h = self.canvas[1] * s;
        Rect {
            x: self.area.x + (self.area.w - w) / 2.0,
            y: self.area.y + (self.area.h - h) / 2.0,
            w,
            h,
        }
    }

    /// A window point in canvas pixels (outside the picture too); `None` while the
    /// viewport is empty.
    pub fn to_canvas(&self, point: [f32; 2]) -> Option<[f32; 2]> {
        let s = self.scale();
        if s <= 0.0 {
            return None;
        }
        let p = self.picture();
        Some([(point[0] - p.x) / s, (point[1] - p.y) / s])
    }

    /// A canvas rectangle in window coordinates.
    pub fn to_view(&self, r: Rect) -> Rect {
        let s = self.scale();
        let p = self.picture();
        Rect {
            x: p.x + r.x * s,
            y: p.y + r.y * s,
            w: r.w * s,
            h: r.h * s,
        }
    }
}

/// What a press on the preview takes hold of.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Grip {
    /// The whole box (a press inside it).
    Move,
    /// A corner or edge handle.
    Handle(Handle),
}

/// The eight handles: corners and edge midpoints of the visible box.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Handle {
    /// Top-left corner.
    TopLeft,
    /// Middle of the top edge.
    Top,
    /// Top-right corner.
    TopRight,
    /// Middle of the right edge.
    Right,
    /// Bottom-right corner.
    BottomRight,
    /// Middle of the bottom edge.
    Bottom,
    /// Bottom-left corner.
    BottomLeft,
    /// Middle of the left edge.
    Left,
}

impl Handle {
    /// Every handle, clockwise from the top-left corner.
    pub const ALL: [Handle; 8] = [
        Handle::TopLeft,
        Handle::Top,
        Handle::TopRight,
        Handle::Right,
        Handle::BottomRight,
        Handle::Bottom,
        Handle::BottomLeft,
        Handle::Left,
    ];

    /// Which edges it moves: x (-1 left, 0 none, 1 right) and y (-1 top, 1 bottom).
    fn sides(self) -> (i8, i8) {
        match self {
            Handle::TopLeft => (-1, -1),
            Handle::Top => (0, -1),
            Handle::TopRight => (1, -1),
            Handle::Right => (1, 0),
            Handle::BottomRight => (1, 1),
            Handle::Bottom => (0, 1),
            Handle::BottomLeft => (-1, 1),
            Handle::Left => (-1, 0),
        }
    }

    /// True for the four corners.
    pub fn is_corner(self) -> bool {
        let (x, y) = self.sides();
        x != 0 && y != 0
    }

    /// Its centre on a rectangle.
    fn centre(self, r: Rect) -> [f32; 2] {
        let (sx, sy) = self.sides();
        let x = match sx {
            -1 => r.x,
            0 => r.x + r.w / 2.0,
            _ => r.x + r.w,
        };
        let y = match sy {
            -1 => r.y,
            0 => r.y + r.h / 2.0,
            _ => r.y + r.h,
        };
        [x, y]
    }
}

/// What the pointer at `point` (window coordinates) would take on `selected`: a handle
/// within reach first (the nearest), then the box itself.
pub fn grip_at(view: &Viewport, selected: &SourceBox, point: [f32; 2]) -> Option<Grip> {
    let r = view.to_view(selected.visual());
    let mut best: Option<(f32, Handle)> = None;
    for h in Handle::ALL {
        let c = h.centre(r);
        let d = (c[0] - point[0]).abs().max((c[1] - point[1]).abs());
        if d <= HANDLE_REACH && best.is_none_or(|(bd, _)| d < bd) {
            best = Some((d, h));
        }
    }
    if let Some((_, h)) = best {
        return Some(Grip::Handle(h));
    }
    contains(r, point).then_some(Grip::Move)
}

/// The topmost box under `point`; `boxes` are bottom first, as the canvas draws them.
pub fn topmost_at(view: &Viewport, boxes: &[SourceBox], point: [f32; 2]) -> Option<usize> {
    boxes
        .iter()
        .rposition(|b| contains(view.to_view(b.visual()), point))
}

fn contains(r: Rect, p: [f32; 2]) -> bool {
    p[0] >= r.x && p[0] <= r.x + r.w && p[1] >= r.y && p[1] <= r.y + r.h
}

/// Options of one pointer move during a drag.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DragKeys {
    /// Corners resize freely instead of keeping the aspect ratio (Shift held).
    pub free: bool,
    /// No snapping to the canvas's edges and centre (Ctrl held; Cmd on macOS).
    pub no_snap: bool,
    /// A handle crops instead of resizing (Alt held; Option on macOS; ADR-0057).
    pub crop: bool,
}

/// A drag that started on a grip of a box.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Drag {
    grip: Grip,
    start: [f32; 2],
    from: SourceBox,
}

impl Drag {
    /// Starts dragging `grip` of `from`, pressed at `point` in canvas pixels.
    pub fn new(grip: Grip, point: [f32; 2], from: SourceBox) -> Self {
        Self {
            grip,
            start: point,
            from,
        }
    }

    /// What it holds.
    pub fn grip(&self) -> Grip {
        self.grip
    }

    /// The box as it started.
    pub fn from(&self) -> SourceBox {
        self.from
    }

    /// The box with the pointer at `point` (canvas pixels). Moving snaps the box's
    /// edges and centre to the canvas's; resizing snaps the moved edges. Corners keep
    /// the aspect ratio unless `keys.free`; edges change one side only. A drag never
    /// turns the box inside out: it stops at [`MIN_SIZE`]. `snap` is the snap distance
    /// in canvas pixels (from [`SNAP_DISTANCE`] and the viewport's scale).
    pub fn update(
        &self,
        point: [f32; 2],
        canvas: [f32; 2],
        snap: f32,
        keys: DragKeys,
    ) -> SourceBox {
        let dx = point[0] - self.start[0];
        let dy = point[1] - self.start[1];
        let snap = if keys.no_snap { 0.0 } else { snap.max(0.0) };
        let r = self.from.visual();
        let out = match self.grip {
            Grip::Move => {
                let x = snap_span(r.x + dx, r.w, canvas[0], snap);
                let y = snap_span(r.y + dy, r.h, canvas[1], snap);
                Rect { x, y, ..r }
            }
            Grip::Handle(h) => resize(r, h, dx, dy, canvas, snap, keys.free),
        };
        self.from.with_visual(out)
    }
}

/// A span starting at `start` of `len` moved so that its start, centre or end lies on
/// a guide (0, the middle, `extent`) when one is within `snap`; the nearest wins.
fn snap_span(start: f32, len: f32, extent: f32, snap: f32) -> f32 {
    if snap <= 0.0 {
        return start;
    }
    let guides = [0.0, extent / 2.0, extent];
    let points = [0.0, len / 2.0, len];
    let mut best: Option<(f32, f32)> = None;
    for g in guides {
        for p in points {
            let shift = g - (start + p);
            if shift.abs() <= snap && best.is_none_or(|(b, _)| shift.abs() < b) {
                best = Some((shift.abs(), shift));
            }
        }
    }
    start + best.map_or(0.0, |(_, s)| s)
}

/// A value moved onto the nearest guide within `snap`.
fn snap_value(v: f32, extent: f32, snap: f32) -> f32 {
    if snap <= 0.0 {
        return v;
    }
    [0.0, extent / 2.0, extent]
        .into_iter()
        .filter(|g| (g - v).abs() <= snap)
        .min_by(|a, b| (a - v).abs().total_cmp(&(b - v).abs()))
        .unwrap_or(v)
}

fn resize(r: Rect, h: Handle, dx: f32, dy: f32, canvas: [f32; 2], snap: f32, free: bool) -> Rect {
    let (sx, sy) = h.sides();
    let (mut x0, mut x1) = (r.x, r.x + r.w);
    let (mut y0, mut y1) = (r.y, r.y + r.h);
    // Moved edges follow the pointer, snap, and stop short of the opposite edge.
    match sx {
        -1 => x0 = snap_value(x0 + dx, canvas[0], snap).min(x1 - MIN_SIZE),
        1 => x1 = snap_value(x1 + dx, canvas[0], snap).max(x0 + MIN_SIZE),
        _ => {}
    }
    match sy {
        -1 => y0 = snap_value(y0 + dy, canvas[1], snap).min(y1 - MIN_SIZE),
        1 => y1 = snap_value(y1 + dy, canvas[1], snap).max(y0 + MIN_SIZE),
        _ => {}
    }
    if h.is_corner() && !free && r.w > 0.0 && r.h > 0.0 {
        // Keep the aspect ratio: the side that changed relatively more wins, and the
        // other follows; the corner opposite the handle stays.
        let aspect = r.w / r.h;
        let (w, hgt) = (x1 - x0, y1 - y0);
        let (w, hgt) = if (w / r.w - 1.0).abs() >= (hgt / r.h - 1.0).abs() {
            (w, w / aspect)
        } else {
            (hgt * aspect, hgt)
        };
        let (w, hgt) = if w < MIN_SIZE || hgt < MIN_SIZE {
            let k = (MIN_SIZE / w).max(MIN_SIZE / hgt);
            (w * k, hgt * k)
        } else {
            (w, hgt)
        };
        if sx < 0 {
            x0 = x1 - w;
        } else {
            x1 = x0 + w;
        }
        if sy < 0 {
            y0 = y1 - hgt;
        } else {
            y1 = y0 + hgt;
        }
    }
    Rect {
        x: x0,
        y: y0,
        w: x1 - x0,
        h: y1 - y0,
    }
}

/// A crop drag (Alt or Option held on a handle, ADR-0057): the crop that makes `from`,
/// a box showing a `picture` (width, height in source pixels) cut by `crop` (left, top,
/// right, bottom), into `to`, a box with the same scale and some edges moved (from
/// [`Drag::update`] with a handle and free corners). Returns the crop and the box it
/// gives. Each moved edge cuts or uncuts source pixels at the box's scale, rounded to
/// whole pixels; a crop never goes below 0 (the picture ends there) and always leaves
/// at least one pixel. A flipped box crops the source edge shown on that side. `None`
/// when `from` has no area or the crop already leaves no picture.
pub fn crop_for(
    from: SourceBox,
    crop: [u32; 4],
    picture: (u32, u32),
    to: SourceBox,
) -> Option<([u32; 4], SourceBox)> {
    let [l, t, r, b] = crop.map(i64::from);
    let (w, h) = (i64::from(picture.0), i64::from(picture.1));
    let (shown_w, shown_h) = (w - l - r, h - t - b);
    let a = from.visual();
    if shown_w < 1 || shown_h < 1 || !(a.w > 0.0 && a.h > 0.0) {
        return None;
    }
    let (kx, ky) = (a.w / shown_w as f32, a.h / shown_h as f32);
    let z = to.visual();
    // Source pixels cut (positive) at each visible edge.
    let px = |canvas: f32, k: f32| (canvas / k).round() as i64;
    let (vl, vr) = (px(z.x - a.x, kx), px((a.x + a.w) - (z.x + z.w), kx));
    let (vt, vb) = (px(z.y - a.y, ky), px((a.y + a.h) - (z.y + z.h), ky));
    // A flipped box shows the source's right (bottom) edge on its left (top).
    let (dl, dr) = if from.size[0] < 0.0 {
        (vr, vl)
    } else {
        (vl, vr)
    };
    let (dt, db) = if from.size[1] < 0.0 {
        (vb, vt)
    } else {
        (vt, vb)
    };
    let side = |c: i64, d: i64, other: i64, total: i64| (c + d).clamp(0, total - 1 - other);
    let nl = side(l, dl, r, w);
    let nr = side(r, dr, nl, w);
    let nt = side(t, dt, b, h);
    let nb = side(b, db, nt, h);
    // The box of that crop at the same scale, measured from the edges that stay.
    let (cl, cr) = if from.size[0] < 0.0 {
        (nr - r, nl - l)
    } else {
        (nl - l, nr - r)
    };
    let (ct, cb) = if from.size[1] < 0.0 {
        (nb - b, nt - t)
    } else {
        (nt - t, nb - b)
    };
    let x0 = a.x + cl as f32 * kx;
    let x1 = a.x + a.w - cr as f32 * kx;
    let y0 = a.y + ct as f32 * ky;
    let y1 = a.y + a.h - cb as f32 * ky;
    let shown = Rect {
        x: x0,
        y: y0,
        w: x1 - x0,
        h: y1 - y0,
    };
    let to_u32 = |v: i64| u32::try_from(v).unwrap_or(0);
    Some((
        [to_u32(nl), to_u32(nt), to_u32(nr), to_u32(nb)],
        from.with_visual(shown),
    ))
}

/// One filled rectangle of the selection drawing, window coordinates.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Mark {
    /// Where.
    pub rect: Rect,
    /// What it is.
    pub kind: MarkKind,
}

/// The parts of the selection drawing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarkKind {
    /// A side of the outline (`color.accent`).
    Outline,
    /// A handle's border (`color.accent`).
    HandleBorder,
    /// A handle's inside (`surface.raised`).
    HandleFill,
}

/// Most marks one selection draws: four outline sides and two per handle.
pub const MAX_MARKS: usize = 4 + 2 * Handle::ALL.len();

/// The selection of `selected` as filled rectangles, written into `out` (cleared
/// first; no allocation once it has [`MAX_MARKS`] capacity). Handles are drawn only
/// when `handles` (a box too small on screen gets the outline only).
pub fn marks(view: &Viewport, selected: &SourceBox, handles: bool, out: &mut Vec<Mark>) {
    out.clear();
    let r = view.to_view(selected.visual());
    if !(r.w.is_finite() && r.h.is_finite() && r.x.is_finite() && r.y.is_finite()) {
        return;
    }
    let t = OUTLINE_WIDTH;
    let outline = [
        Rect {
            x: r.x,
            y: r.y,
            w: r.w,
            h: t,
        },
        Rect {
            x: r.x,
            y: r.y + r.h - t,
            w: r.w,
            h: t,
        },
        Rect {
            x: r.x,
            y: r.y,
            w: t,
            h: r.h,
        },
        Rect {
            x: r.x + r.w - t,
            y: r.y,
            w: t,
            h: r.h,
        },
    ];
    out.extend(outline.into_iter().map(|rect| Mark {
        rect,
        kind: MarkKind::Outline,
    }));
    if !handles || r.w < 3.0 * HANDLE_SIZE || r.h < 3.0 * HANDLE_SIZE {
        return;
    }
    for h in Handle::ALL {
        let [cx, cy] = h.centre(r);
        let s = HANDLE_SIZE;
        let outer = Rect {
            x: cx - s / 2.0,
            y: cy - s / 2.0,
            w: s,
            h: s,
        };
        out.push(Mark {
            rect: outer,
            kind: MarkKind::HandleBorder,
        });
        out.push(Mark {
            rect: Rect {
                x: outer.x + t,
                y: outer.y + t,
                w: s - 2.0 * t,
                h: s - 2.0 * t,
            },
            kind: MarkKind::HandleFill,
        });
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn view() -> Viewport {
        // A 1920 x 1080 canvas in a 1000 x 1000 panel: scale 1000 / 1920, bars above and
        // below.
        Viewport {
            area: Rect::sized(1000.0, 1000.0),
            canvas: [1920.0, 1080.0],
        }
    }

    #[test]
    fn the_picture_is_letterboxed_and_points_map_both_ways() {
        let v = view();
        let p = v.picture();
        assert!((p.w - 1000.0).abs() < 1e-3);
        assert!((p.h - 562.5).abs() < 1e-3);
        assert!((p.y - 218.75).abs() < 1e-3);
        let c = v.to_canvas([500.0, 500.0]).unwrap();
        assert!((c[0] - 960.0).abs() < 1e-3 && (c[1] - 540.0).abs() < 1e-3);
        let r = v.to_view(Rect::sized(1920.0, 1080.0));
        assert!((r.w - p.w).abs() < 1e-3 && (r.y - p.y).abs() < 1e-3);
        let empty = Viewport {
            area: Rect::sized(0.0, 0.0),
            canvas: [1920.0, 1080.0],
        };
        assert_eq!(empty.to_canvas([1.0, 1.0]), None);
    }

    #[test]
    fn handles_win_over_the_inside_and_outside_is_nothing() {
        let v = Viewport {
            area: Rect::sized(1920.0, 1080.0),
            canvas: [1920.0, 1080.0],
        };
        let b = SourceBox::new([100.0, 100.0], [400.0, 200.0]);
        assert_eq!(grip_at(&v, &b, [300.0, 200.0]), Some(Grip::Move));
        assert_eq!(
            grip_at(&v, &b, [103.0, 98.0]),
            Some(Grip::Handle(Handle::TopLeft))
        );
        assert_eq!(
            grip_at(&v, &b, [500.0, 200.0]),
            Some(Grip::Handle(Handle::Right))
        );
        assert_eq!(grip_at(&v, &b, [700.0, 700.0]), None);
        // Flipped boxes have handles where they show.
        let flipped = SourceBox::new([500.0, 100.0], [-400.0, 200.0]);
        assert_eq!(
            grip_at(&v, &flipped, [100.0, 100.0]),
            Some(Grip::Handle(Handle::TopLeft))
        );
    }

    #[test]
    fn the_topmost_box_is_picked() {
        let v = Viewport {
            area: Rect::sized(1920.0, 1080.0),
            canvas: [1920.0, 1080.0],
        };
        let screen = SourceBox::new([0.0, 0.0], [1920.0, 1080.0]);
        let camera = SourceBox::new([1400.0, 700.0], [480.0, 270.0]);
        assert_eq!(topmost_at(&v, &[screen, camera], [1500.0, 800.0]), Some(1));
        assert_eq!(topmost_at(&v, &[screen, camera], [100.0, 100.0]), Some(0));
        assert_eq!(topmost_at(&v, &[camera, screen], [1500.0, 800.0]), Some(1));
        assert_eq!(topmost_at(&v, &[camera], [10.0, 10.0]), None);
    }

    const CANVAS: [f32; 2] = [1920.0, 1080.0];

    #[test]
    fn moving_follows_the_pointer_and_snaps_to_edges_and_centre() {
        let b = SourceBox::new([100.0, 100.0], [400.0, 200.0]);
        let d = Drag::new(Grip::Move, [200.0, 200.0], b);
        let keys = DragKeys::default();
        // Far from every guide: exact.
        let out = d.update([450.0, 350.0], CANVAS, 10.0, keys);
        assert_eq!(out, SourceBox::new([350.0, 250.0], [400.0, 200.0]));
        // Left edge 6 px from the canvas edge: snaps to 0.
        let out = d.update([106.0, 350.0], CANVAS, 10.0, keys);
        assert_eq!(out.position[0], 0.0);
        // Centre near the canvas centre: snaps to it.
        let out = d.update([862.0, 642.0], CANVAS, 10.0, keys);
        assert_eq!(out.position, [760.0, 540.0]);
        // Ctrl: no snapping.
        let free = DragKeys {
            no_snap: true,
            ..keys
        };
        let out = d.update([106.0, 350.0], CANVAS, 10.0, free);
        assert_eq!(out.position[0], 6.0);
    }

    #[test]
    fn corners_keep_the_aspect_ratio_unless_free() {
        let b = SourceBox::new([100.0, 100.0], [400.0, 200.0]);
        let d = Drag::new(Grip::Handle(Handle::BottomRight), [500.0, 300.0], b);
        let out = d.update([700.0, 320.0], CANVAS, 0.0, DragKeys::default());
        // Width grew most: 600 wide, so 300 high; the top-left corner stays.
        assert_eq!(out, SourceBox::new([100.0, 100.0], [600.0, 300.0]));
        let free = DragKeys {
            free: true,
            ..DragKeys::default()
        };
        let out = d.update([700.0, 320.0], CANVAS, 0.0, free);
        assert_eq!(out, SourceBox::new([100.0, 100.0], [600.0, 220.0]));
        // The top-left corner keeps the bottom-right corner fixed.
        let d = Drag::new(Grip::Handle(Handle::TopLeft), [100.0, 100.0], b);
        let out = d.update([300.0, 100.0], CANVAS, 0.0, DragKeys::default());
        assert_eq!(out, SourceBox::new([300.0, 200.0], [200.0, 100.0]));
    }

    #[test]
    fn edges_change_one_side_and_never_turn_the_box_inside_out() {
        let b = SourceBox::new([100.0, 100.0], [400.0, 200.0]);
        let d = Drag::new(Grip::Handle(Handle::Right), [500.0, 200.0], b);
        let out = d.update([600.0, 900.0], CANVAS, 0.0, DragKeys::default());
        assert_eq!(out, SourceBox::new([100.0, 100.0], [500.0, 200.0]));
        let out = d.update([-900.0, 200.0], CANVAS, 0.0, DragKeys::default());
        assert_eq!(out, SourceBox::new([100.0, 100.0], [MIN_SIZE, 200.0]));
        let d = Drag::new(Grip::Handle(Handle::TopLeft), [100.0, 100.0], b);
        let out = d.update([5000.0, 5000.0], CANVAS, 0.0, DragKeys::default());
        assert!(
            out.size[0] >= MIN_SIZE && out.size[1] >= MIN_SIZE,
            "{out:?}"
        );
        assert!((out.size[0] / out.size[1] - 2.0).abs() < 1e-3);
        // The bottom-right corner stays where it was.
        assert!((out.position[0] + out.size[0] - 500.0).abs() < 1e-3);
        assert!((out.position[1] + out.size[1] - 300.0).abs() < 1e-3);
    }

    #[test]
    fn a_flipped_box_stays_flipped() {
        let b = SourceBox::new([500.0, 100.0], [-400.0, 200.0]);
        let d = Drag::new(Grip::Handle(Handle::Right), [500.0, 200.0], b);
        let out = d.update([600.0, 200.0], CANVAS, 0.0, DragKeys::default());
        assert_eq!(out, SourceBox::new([600.0, 100.0], [-500.0, 200.0]));
        let m = Drag::new(Grip::Move, [0.0, 0.0], b).update(
            [10.0, 0.0],
            CANVAS,
            0.0,
            DragKeys::default(),
        );
        assert_eq!(m, SourceBox::new([510.0, 100.0], [-400.0, 200.0]));
    }

    #[test]
    fn resized_edges_snap_to_the_canvas() {
        let b = SourceBox::new([100.0, 100.0], [400.0, 200.0]);
        let d = Drag::new(Grip::Handle(Handle::Right), [500.0, 200.0], b);
        let out = d.update([1915.0, 200.0], CANVAS, 10.0, DragKeys::default());
        assert_eq!(out.size[0], 1820.0);
    }

    #[test]
    fn marks_draw_outline_and_handles_without_allocating() {
        let v = Viewport {
            area: Rect::sized(1920.0, 1080.0),
            canvas: [1920.0, 1080.0],
        };
        let mut out = Vec::with_capacity(MAX_MARKS);
        let cap = out.capacity();
        marks(
            &v,
            &SourceBox::new([100.0, 100.0], [400.0, 200.0]),
            true,
            &mut out,
        );
        assert_eq!(out.len(), MAX_MARKS);
        assert_eq!(out.capacity(), cap);
        assert_eq!(
            out.iter().filter(|m| m.kind == MarkKind::Outline).count(),
            4
        );
        // Too small on screen: outline only.
        marks(
            &v,
            &SourceBox::new([100.0, 100.0], [10.0, 10.0]),
            true,
            &mut out,
        );
        assert_eq!(out.len(), 4);
        marks(
            &v,
            &SourceBox::new([f32::NAN, 0.0], [1.0, 1.0]),
            true,
            &mut out,
        );
        assert!(out.is_empty());
    }

    #[test]
    fn a_crop_drag_cuts_source_pixels_at_the_box_scale() {
        // A 3440 x 1440 picture shown at half size.
        let from = SourceBox::new([100.0, 50.0], [1720.0, 720.0]);
        let picture = (3440, 1440);
        // The left edge 380 canvas px in: 760 source px cut, the right edge stays.
        let to = SourceBox::new([480.0, 50.0], [1340.0, 720.0]);
        let (crop, shown) = crop_for(from, [0; 4], picture, to).unwrap();
        assert_eq!(crop, [760, 0, 0, 0]);
        assert_eq!(shown, to);
        // Dragging past the picture's edge uncuts only what was cut.
        let back = SourceBox::new([0.0, 50.0], [1820.0, 720.0]);
        let (crop, shown) = crop_for(to, [760, 0, 0, 0], picture, back).unwrap();
        assert_eq!(crop, [0; 4]);
        assert_eq!(shown, from);
    }

    #[test]
    fn a_crop_drag_leaves_one_pixel_and_follows_flips() {
        let picture = (100, 100);
        let from = SourceBox::new([0.0, 0.0], [100.0, 100.0]);
        // The bottom edge dragged above the top: one row stays.
        let to = SourceBox::new([0.0, 0.0], [100.0, -50.0]);
        let (crop, shown) = crop_for(from, [0; 4], picture, to).unwrap();
        assert_eq!(crop, [0, 0, 0, 99]);
        assert_eq!(shown.visual().h, 1.0);
        // Flipped left to right: the visible left edge is the source's right edge.
        let flipped = SourceBox::new([100.0, 0.0], [-100.0, 100.0]);
        let to = flipped.with_visual(Rect {
            x: 10.0,
            y: 0.0,
            w: 90.0,
            h: 100.0,
        });
        let (crop, _) = crop_for(flipped, [0; 4], picture, to).unwrap();
        assert_eq!(crop, [0, 0, 10, 0]);
        // Nothing to show already: no crop.
        assert!(crop_for(from, [50, 0, 50, 0], picture, from).is_none());
    }
}
