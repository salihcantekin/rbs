//! What of the preview stays visible around toolkit content drawn over it (ADR-0054).
//!
//! Platform-neutral arithmetic of the preview layers, kept apart so it is tested on
//! every platform. Rectangles are edges (`x0 < x1`, `y0 < y1`) in the layer's window
//! coordinates with the origin at the top left.

#![cfg_attr(
    not(any(windows, target_os = "macos")),
    allow(dead_code, reason = "used by the platform layers")
)]

/// Most covers a layer cuts out of the picture and its marks; with more, the layer hides
/// (ADR-0054 decision 3 names five as the most in practice).
pub const MAX_CUTOUTS: usize = 8;
/// Most tiles the picture is split into (ADR-0054 decision 3): exact for five covers,
/// `(2 * 5 + 1) * (5 + 1) = 66`.
pub(crate) const MAX_TILES: usize = 72;
/// Most pieces the marks are drawn as while covers cut them (ADR-0054 decision 3).
pub(crate) const MAX_MARK_PIECES: usize = 128;
/// Pieces one subtraction can make with [`MAX_CUTOUTS`] covers: `(2k + 1) * (k + 1)`.
pub(crate) const MAX_PIECES: usize = (2 * MAX_CUTOUTS + 1) * (MAX_CUTOUTS + 1);

/// An axis-aligned rectangle by its edges.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Edges {
    pub x0: f64,
    pub y0: f64,
    pub x1: f64,
    pub y1: f64,
}

impl Edges {
    /// From a position and a size; `None` when not finite.
    pub fn from_rect(x: f64, y: f64, width: f64, height: f64) -> Option<Self> {
        let e = Self {
            x0: x,
            y0: y,
            x1: x + width,
            y1: y + height,
        };
        [e.x0, e.y0, e.x1, e.y1]
            .iter()
            .all(|v| v.is_finite())
            .then_some(e)
    }

    /// Whether it has an area.
    pub fn is_empty(&self) -> bool {
        use std::cmp::Ordering::Greater;
        self.x1.partial_cmp(&self.x0) != Some(Greater)
            || self.y1.partial_cmp(&self.y0) != Some(Greater)
    }

    /// Whether `self` and `other` share an area.
    pub fn overlaps(&self, other: &Edges) -> bool {
        !self.is_empty()
            && !other.is_empty()
            && self.x0 < other.x1
            && other.x0 < self.x1
            && self.y0 < other.y1
            && other.y0 < self.y1
    }

    /// Width times height.
    pub fn area(&self) -> f64 {
        (self.x1 - self.x0).max(0.0) * (self.y1 - self.y0).max(0.0)
    }

    /// Grown to whole device pixels of `scale` (device pixels per unit): start floored,
    /// end ceiled, so no part of it is left out.
    pub fn snapped_outward(&self, scale: f64) -> Self {
        if scale <= 0.0 || scale.is_nan() {
            return *self;
        }
        Self {
            x0: (self.x0 * scale).floor() / scale,
            y0: (self.y0 * scale).floor() / scale,
            x1: (self.x1 * scale).ceil() / scale,
            y1: (self.y1 * scale).ceil() / scale,
        }
    }
}

/// The picture's rectangle: an image of `image` pixels fitted into `area` keeping its
/// aspect ratio and centred, edges rounded to device pixels of `scale` (ADR-0054
/// decision 3). Both layers draw the picture here, and the region is cut from it.
pub(crate) fn picture(area: Edges, image: (u32, u32), scale: f64) -> Edges {
    let (x, y, s) = super::geometry::fit(
        (area.x0, area.y0, area.x1 - area.x0, area.y1 - area.y0),
        image,
    );
    let w = f64::from(image.0) * s;
    let h = f64::from(image.1) * s;
    let snap = |v: f64| super::geometry::snap(v, scale);
    Edges {
        x0: snap(x),
        y0: snap(y),
        x1: snap(x + w),
        y1: snap(y + h),
    }
}

/// Replaces `out` with what of `rect` no cover overlaps, as disjoint rectangles:
/// horizontal bands at every cover edge, each band's uncovered spans, a span equal in x
/// to one in the band above merged with it. At most `cap` pieces; when more are needed
/// the smallest are left out. With at most [`MAX_CUTOUTS`] covers it allocates nothing
/// once `out` holds [`MAX_PIECES`]. Covers beyond [`MAX_CUTOUTS`] are ignored (callers
/// hide instead). Every edge is an edge of `rect` or of a cover.
pub(crate) fn subtract(rect: Edges, covers: &[Edges], cap: usize, out: &mut Vec<Edges>) {
    out.clear();
    if rect.is_empty() {
        return;
    }
    // The covers that cut `rect`, clipped to it.
    let mut cuts = [rect; MAX_CUTOUTS];
    let mut n = 0;
    for c in covers
        .iter()
        .filter(|c| c.overlaps(&rect))
        .take(MAX_CUTOUTS)
    {
        if let Some(slot) = cuts.get_mut(n) {
            *slot = Edges {
                x0: c.x0.max(rect.x0),
                y0: c.y0.max(rect.y0),
                x1: c.x1.min(rect.x1),
                y1: c.y1.min(rect.y1),
            };
            n += 1;
        }
    }
    let cuts = cuts.get(..n).unwrap_or(&[]);
    if cuts.is_empty() {
        out.push(rect);
        return;
    }
    // Band edges: the rectangle's and every cut's, sorted and unique.
    let mut ys = [0.0f64; 2 * MAX_CUTOUTS + 2];
    let mut m = 0;
    for y in [rect.y0, rect.y1]
        .into_iter()
        .chain(cuts.iter().flat_map(|c| [c.y0, c.y1]))
    {
        if let Some(slot) = ys.get_mut(m) {
            *slot = y;
            m += 1;
        }
    }
    let ys = ys.get_mut(..m).unwrap_or(&mut []);
    ys.sort_by(f64::total_cmp);
    let mut last_y = None;
    for &y in ys.iter() {
        let Some(top) = last_y.replace(y) else {
            continue;
        };
        if y <= top {
            continue;
        }
        let bottom = y;
        // The cuts across this band, by left edge.
        let mut across = [rect; MAX_CUTOUTS];
        let mut k = 0;
        for c in cuts.iter().filter(|c| c.y0 < bottom && c.y1 > top) {
            if let Some(slot) = across.get_mut(k) {
                *slot = *c;
                k += 1;
            }
        }
        let across = across.get_mut(..k).unwrap_or(&mut []);
        across.sort_by(|a, b| a.x0.total_cmp(&b.x0));
        let mut x = rect.x0;
        let span = |x0: f64, x1: f64, out: &mut Vec<Edges>| {
            if x1 <= x0 {
                return;
            }
            // Merge with the piece ending at this band's top with the same span (at most
            // one: pieces are disjoint).
            let above = out
                .iter_mut()
                .find(|p| p.x0 == x0 && p.x1 == x1 && p.y1 == top);
            match above {
                Some(p) => p.y1 = bottom,
                None => out.push(Edges {
                    x0,
                    y0: top,
                    x1,
                    y1: bottom,
                }),
            }
        };
        for c in across.iter() {
            span(x, c.x0, out);
            x = x.max(c.x1);
        }
        span(x, rect.x1, out);
    }
    if out.len() > cap {
        out.sort_by(|a, b| b.area().total_cmp(&a.area()));
        out.truncate(cap);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(x0: f64, y0: f64, x1: f64, y1: f64) -> Edges {
        Edges { x0, y0, x1, y1 }
    }

    /// A small deterministic generator (tests stay reproducible, ADR-0032).
    struct Lcg(u64);
    impl Lcg {
        fn next(&mut self, below: u64) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (self.0 >> 33) % below
        }
    }

    fn covered(p: (f64, f64), covers: &[Edges]) -> bool {
        covers
            .iter()
            .any(|c| p.0 > c.x0 && p.0 < c.x1 && p.1 > c.y0 && p.1 < c.y1)
    }

    #[test]
    fn no_cover_gives_the_rectangle() {
        let r = e(10.0, 20.0, 650.0, 380.0);
        let mut out = Vec::new();
        subtract(r, &[], MAX_TILES, &mut out);
        assert_eq!(out, vec![r]);
        // A cover beside it, touching an edge.
        subtract(r, &[e(650.0, 0.0, 900.0, 500.0)], MAX_TILES, &mut out);
        assert_eq!(out, vec![r]);
    }

    #[test]
    fn a_popup_over_one_corner_leaves_two_pieces() {
        let r = e(0.0, 0.0, 100.0, 100.0);
        let mut out = Vec::new();
        subtract(r, &[e(60.0, 60.0, 200.0, 200.0)], MAX_TILES, &mut out);
        assert_eq!(
            out,
            vec![e(0.0, 0.0, 100.0, 60.0), e(0.0, 60.0, 60.0, 100.0)]
        );
    }

    #[test]
    fn a_popup_inside_leaves_four_pieces_and_spans_merge_down() {
        let r = e(0.0, 0.0, 100.0, 100.0);
        let mut out = Vec::new();
        subtract(r, &[e(40.0, 40.0, 60.0, 60.0)], MAX_TILES, &mut out);
        assert_eq!(
            out,
            vec![
                e(0.0, 0.0, 100.0, 40.0),
                e(0.0, 40.0, 40.0, 60.0),
                e(60.0, 40.0, 100.0, 60.0),
                e(0.0, 60.0, 100.0, 100.0),
            ]
        );
        // Two covers stacked in one column: the left and right spans run through.
        subtract(
            r,
            &[e(40.0, 10.0, 60.0, 30.0), e(40.0, 30.0, 60.0, 50.0)],
            MAX_TILES,
            &mut out,
        );
        assert!(out.contains(&e(0.0, 10.0, 40.0, 50.0)), "{out:?}");
        assert!(out.contains(&e(60.0, 10.0, 100.0, 50.0)), "{out:?}");
    }

    #[test]
    fn fully_covered_gives_nothing() {
        let mut out = Vec::new();
        subtract(
            e(0.0, 0.0, 10.0, 10.0),
            &[e(-1.0, -1.0, 11.0, 11.0)],
            MAX_TILES,
            &mut out,
        );
        assert!(out.is_empty());
    }

    /// Properties over generated covers: disjoint pieces inside the rectangle, none over
    /// a cover, every uncovered point covered, edges from the inputs, within the bound.
    #[test]
    fn region_properties_over_generated_covers() {
        let r = e(0.0, 0.0, 64.0, 36.0);
        let mut rng = Lcg(7);
        let mut out = Vec::new();
        for round in 0..500 {
            let k = (round % 5) + 1;
            let covers: Vec<Edges> = (0..k)
                .map(|_| {
                    let x0 = rng.next(80) as f64 - 8.0;
                    let y0 = rng.next(50) as f64 - 7.0;
                    e(
                        x0,
                        y0,
                        x0 + 1.0 + rng.next(40) as f64,
                        y0 + 1.0 + rng.next(25) as f64,
                    )
                })
                .collect();
            subtract(r, &covers, MAX_TILES, &mut out);
            let bound = (2 * k + 1) * (k + 1);
            assert!(out.len() <= bound, "{} pieces for {k} covers", out.len());
            for (i, a) in out.iter().enumerate() {
                assert!(!a.is_empty());
                assert!(a.x0 >= r.x0 && a.x1 <= r.x1 && a.y0 >= r.y0 && a.y1 <= r.y1);
                for c in &covers {
                    assert!(!a.overlaps(c), "{a:?} overlaps cover {c:?}");
                }
                for b in out.iter().skip(i + 1) {
                    assert!(!a.overlaps(b), "{a:?} overlaps {b:?}");
                }
            }
            // Every half-pixel sample outside the covers is in exactly one piece.
            for yi in 0..36 {
                for xi in 0..64 {
                    let p = (xi as f64 + 0.5, yi as f64 + 0.5);
                    let n = out
                        .iter()
                        .filter(|a| p.0 > a.x0 && p.0 < a.x1 && p.1 > a.y0 && p.1 < a.y1)
                        .count();
                    let want = usize::from(!covered(p, &covers));
                    assert_eq!(n, want, "round {round} at {p:?}: {covers:?} -> {out:?}");
                }
            }
        }
    }

    #[test]
    fn over_the_cap_the_smallest_pieces_go() {
        let r = e(0.0, 0.0, 100.0, 100.0);
        let mut out = Vec::new();
        let covers = [e(10.0, 10.0, 20.0, 20.0), e(50.0, 50.0, 60.0, 60.0)];
        subtract(r, &covers, usize::MAX, &mut out);
        let all = out.clone();
        subtract(r, &covers, 3, &mut out);
        assert_eq!(out.len(), 3);
        let smallest_kept = out.iter().map(Edges::area).fold(f64::MAX, f64::min);
        let dropped_max = all
            .iter()
            .filter(|a| !out.contains(a))
            .map(Edges::area)
            .fold(0.0, f64::max);
        assert!(smallest_kept >= dropped_max);
    }

    #[test]
    fn covers_snap_outward_and_the_picture_fits_on_pixels() {
        let c = e(10.2, 10.7, 20.2, 20.3).snapped_outward(2.0);
        assert_eq!(c, e(10.0, 10.5, 20.5, 20.5));
        assert_eq!(
            e(1.0, 1.0, 2.0, 2.0).snapped_outward(0.0),
            e(1.0, 1.0, 2.0, 2.0)
        );
        assert!(Edges::from_rect(f64::NAN, 0.0, 1.0, 1.0).is_none());
        // 16:9 into a 4:3 area at 2x: letterboxed, edges on half points.
        let p = picture(e(0.0, 0.0, 401.0, 300.0), (1280, 720), 2.0);
        assert_eq!((p.x0, p.x1), (0.0, 401.0));
        assert_eq!(p.y0 * 2.0, (p.y0 * 2.0).round());
        assert_eq!(p.y1 * 2.0, (p.y1 * 2.0).round());
    }
}
