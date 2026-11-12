//! Crop edits that keep the picture's scale (ADR-0058), and the source-region
//! size those edits use (ADR-0103, ADR-0104).

use crate::arrange::FitMode;
use crate::flatten::Affine2;
use crate::model::{ChromeFill, Crop, DisplayChrome, Layout, SourceRegion};

/// The picture size fit and recrop use: the region size when `region` is set,
/// otherwise `picture`. The item crop is then taken inside that rectangle.
/// `None` when the intersection leaves no pixel.
pub fn visible_size(
    picture: (u32, u32),
    region: Option<SourceRegion>,
    crop: Crop,
) -> Option<(u32, u32)> {
    let (base, crop) = match region {
        Some(region) => (region.size(picture)?, region.crop_inside(crop)),
        None => (picture, crop),
    };
    if !crop.fits(base.0, base.1) {
        return None;
    }
    Some((
        base.0 - crop.left - crop.right,
        base.1 - crop.top - crop.bottom,
    ))
}

/// [`Layout::recrop`] against the region size when a region is set.
///
/// The crop stored on the layout stays the full-picture item crop. The box is
/// computed from the region size and the part of the item crop that lies inside
/// the region, so a source pixel of the sampled picture keeps its place.
pub fn recrop_keeping(
    layout: Layout,
    picture: (u32, u32),
    region: Option<SourceRegion>,
    crop: Crop,
) -> Option<Layout> {
    let (base, old_crop, new_crop) = match region {
        Some(region) => (
            region.size(picture)?,
            region.crop_inside(layout.crop),
            region.crop_inside(crop),
        ),
        None => (picture, layout.crop, crop),
    };
    let mut adjusted = layout;
    adjusted.crop = old_crop;
    let mut out = adjusted.recrop(base, new_crop)?;
    out.crop = crop;
    Some(out)
}

/// Places `layout`'s box with the chrome fill, using the region size as the
/// picture. The crop is unchanged. `None` when the region leaves no pixel.
/// Cover and fit are layout placements, not a compositor scale (ADR-0104).
pub fn place_chrome(
    chrome: DisplayChrome,
    canvas: (u32, u32),
    picture: (u32, u32),
    layout: Layout,
) -> Option<Layout> {
    let size = chrome.region().size(picture)?;
    let mode = match chrome.fill {
        ChromeFill::Cover => FitMode::Fill,
        ChromeFill::Fit => FitMode::Fit,
    };
    layout.fitted(canvas, size, mode)
}

impl Crop {
    /// True when the crop leaves at least one pixel in each direction of a
    /// `width x height` picture.
    pub fn fits(&self, width: u32, height: u32) -> bool {
        u64::from(self.left) + u64::from(self.right) < u64::from(width)
            && u64::from(self.top) + u64::from(self.bottom) < u64::from(height)
    }
}

impl Layout {
    /// The same layout with `crop` on a `source` (width, height) picture in pixels,
    /// keeping the size of a source pixel on the canvas and the place of the part of
    /// the picture that stays (rotation and flips included). `None` when `crop` leaves
    /// less than one pixel, `source` is empty, or a value is not finite.
    ///
    /// If the current crop does not fit `source` (the picture shrank), the scale is
    /// taken from the whole picture and the new box keeps the current centre.
    pub fn recrop(&self, source: (u32, u32), crop: Crop) -> Option<Layout> {
        let (w, h) = source;
        if !crop.fits(w, h) {
            return None;
        }
        let (wf, hf) = (w as f64, h as f64);
        let old = self.crop;
        // The new picture's span in the unit square of the current (cropped) picture.
        let (a0, a1, b0, b1) = if old.fits(w, h) {
            let w0 = wf - f64::from(old.left) - f64::from(old.right);
            let h0 = hf - f64::from(old.top) - f64::from(old.bottom);
            (
                (f64::from(crop.left) - f64::from(old.left)) / w0,
                (wf - f64::from(crop.right) - f64::from(old.left)) / w0,
                (f64::from(crop.top) - f64::from(old.top)) / h0,
                (hf - f64::from(crop.bottom) - f64::from(old.top)) / h0,
            )
        } else {
            let nw = wf - f64::from(crop.left) - f64::from(crop.right);
            let nh = hf - f64::from(crop.top) - f64::from(crop.bottom);
            let (a, b) = (nw / wf, nh / hf);
            (0.5 - a / 2.0, 0.5 + a / 2.0, 0.5 - b / 2.0, 0.5 + b / 2.0)
        };
        let size = [
            (f64::from(self.size[0]) * (a1 - a0)) as f32,
            (f64::from(self.size[1]) * (b1 - b0)) as f32,
        ];
        let (cx, cy) =
            Affine2::of_layout(self).apply(((a0 + a1) / 2.0) as f32, ((b0 + b1) / 2.0) as f32);
        let position = [cx - size[0] / 2.0, cy - size[1] / 2.0];
        if !position.iter().chain(size.iter()).all(|v| v.is_finite()) {
            return None;
        }
        let mut out = *self;
        out.position = position;
        out.size = size;
        out.crop = crop;
        Some(out)
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)] // Tests fail by panicking.
mod tests {
    use super::*;

    fn close(a: [f32; 2], b: [f32; 2]) -> bool {
        (a[0] - b[0]).abs() < 1e-3 && (a[1] - b[1]).abs() < 1e-3
    }

    /// Where a source pixel (x, y) of a `source` picture lands on the canvas.
    fn pixel(l: &Layout, source: (u32, u32), x: f32, y: f32) -> [f32; 2] {
        let c = l.crop;
        let w0 = source.0 as f32 - c.left as f32 - c.right as f32;
        let h0 = source.1 as f32 - c.top as f32 - c.bottom as f32;
        let (px, py) =
            Affine2::of_layout(l).apply((x - c.left as f32) / w0, (y - c.top as f32) / h0);
        [px, py]
    }

    #[test]
    fn a_crop_keeps_the_scale_and_the_place_of_what_stays() {
        // A 3440x1440 display fitted to a 1920 wide canvas.
        let src = (3440, 1440);
        let l = Layout::new([0.0, 0.0], [1920.0, 803.72]);
        let crop = Crop::new(760, 180, 760, 180);
        let n = l.recrop(src, crop).expect("fits");
        for (x, y) in [(760.0, 180.0), (2680.0, 1260.0), (1720.0, 720.0)] {
            assert!(close(pixel(&l, src, x, y), pixel(&n, src, x, y)), "{x},{y}");
        }
        assert!((n.size[0] - 1920.0 * 1920.0 / 3440.0).abs() < 1e-2);
        assert_eq!(n.crop, crop);
    }

    #[test]
    fn rotated_and_flipped_pictures_stay_in_place() {
        let src = (1000, 500);
        let l = Layout::new([100.0, 50.0], [-400.0, 200.0])
            .with_rotation(30.0)
            .with_crop(Crop::new(100, 0, 0, 50));
        let n = l.recrop(src, Crop::new(300, 100, 200, 0)).expect("fits");
        for (x, y) in [(300.0, 100.0), (800.0, 450.0), (550.0, 275.0)] {
            assert!(close(pixel(&l, src, x, y), pixel(&n, src, x, y)), "{x},{y}");
        }
        assert!(n.size[0] < 0.0, "the flip stays");
        assert_eq!(n.rotation, 30.0);
    }

    #[test]
    fn removing_the_crop_grows_the_box_back() {
        let src = (1920, 1080);
        let full = Layout::new([0.0, 0.0], [1920.0, 1080.0]);
        let cut = full.recrop(src, Crop::new(960, 0, 0, 540)).expect("fits");
        assert!(close(cut.position, [960.0, 0.0]));
        assert!(close(cut.size, [960.0, 540.0]));
        let back = cut.recrop(src, Crop::default()).expect("fits");
        assert!(close(back.position, full.position));
        assert!(close(back.size, full.size));
    }

    #[test]
    fn a_crop_that_leaves_no_picture_is_refused() {
        let l = Layout::new([0.0, 0.0], [100.0, 100.0]);
        assert!(l.recrop((100, 100), Crop::new(50, 0, 50, 0)).is_none());
        assert!(l.recrop((100, 100), Crop::new(0, 99, 0, 0)).is_some());
        assert!(l.recrop((0, 100), Crop::default()).is_none());
        assert!(
            l.recrop((100, 100), Crop::new(u32::MAX, 0, u32::MAX, 0))
                .is_none()
        );
    }

    #[test]
    fn a_region_sizes_the_picture_and_intersects_the_item_crop() {
        use crate::model::SourceRegion;

        let picture = (1920, 1080);
        let region = SourceRegion::new(0, 40, 0, 80);
        assert_eq!(region.size(picture), Some((1920, 960)));
        // A picture smaller than the insets leaves nothing; the stored region stays.
        assert_eq!(region.size((100, 50)), None);
        let item = Crop::new(10, 0, 0, 100);
        assert_eq!(region.intersect_crop(item), Crop::new(10, 40, 0, 100));
        assert_eq!(region.crop_inside(item), Crop::new(10, 0, 0, 20));
        assert_eq!(visible_size(picture, Some(region), item), Some((1910, 940)));
        assert_eq!(visible_size(picture, None, item), Some((1910, 980)));
    }

    #[test]
    fn recrop_keeps_the_full_picture_crop_and_the_region_scale() {
        let picture = (1000, 500);
        let region = SourceRegion::new(100, 0, 100, 0);
        let layout = Layout::new([0.0, 0.0], [800.0, 500.0]);
        let next =
            recrop_keeping(layout, picture, Some(region), Crop::new(200, 0, 100, 0)).expect("fits");
        assert_eq!(next.crop, Crop::new(200, 0, 100, 0));
        // Region is 800 wide. The extra 100 on the left shrinks the box by 100/800.
        assert!((next.size[0] - 700.0).abs() < 1e-2);
        assert!((next.position[0] - 100.0).abs() < 1e-2);
    }

    #[test]
    fn chrome_insets_become_the_region_and_a_missing_work_area_is_absent() {
        use crate::model::ChromeFill;

        let picture = (1920, 1080);
        assert!(DisplayChrome::from_work(picture, None, ChromeFill::Cover).is_none());
        let chrome = DisplayChrome::from_work(picture, Some((0, 25, 1920, 1000)), ChromeFill::Fit)
            .expect("work area");
        assert_eq!(chrome.region(), SourceRegion::new(0, 25, 0, 55));
        assert_eq!(chrome.fill, ChromeFill::Fit);
        assert_eq!(chrome.region().size(picture), Some((1920, 1000)));
        // Menu bar and Dock are two edges of the same four fields.
        assert_eq!(
            (chrome.top, chrome.bottom, chrome.left, chrome.right),
            (25, 55, 0, 0)
        );
    }

    #[test]
    fn cover_and_fit_place_the_box_and_leave_the_crop() {
        use crate::model::ChromeFill;

        let chrome = DisplayChrome::new(40, 0, 0, 0).with_fill(ChromeFill::Cover);
        let layout = Layout::new([10.0, 10.0], [100.0, 80.0]).with_crop(Crop::new(1, 2, 3, 4));
        let cover = place_chrome(chrome, (1920, 1080), (1920, 1080), layout).expect("place");
        assert_eq!(cover.crop, layout.crop);
        assert!(cover.size[0] >= 1920.0 - 1e-2);
        assert!(cover.size[1] >= 1080.0 - 1e-2);
        let fit = place_chrome(
            chrome.with_fill(ChromeFill::Fit),
            (1920, 1080),
            (1920, 1080),
            layout,
        )
        .expect("place");
        assert_eq!(fit.crop, layout.crop);
        assert!(fit.size[0] <= 1920.0 + 1e-2);
        assert!(fit.size[1] <= 1080.0 + 1e-2);
        assert!(fit.size[0] < cover.size[0] || fit.size[1] < cover.size[1]);
    }

    #[test]
    fn an_old_crop_larger_than_the_picture_keeps_the_centre() {
        let l = Layout::new([0.0, 0.0], [200.0, 100.0]).with_crop(Crop::new(500, 0, 500, 0));
        let n = l
            .recrop((800, 400), Crop::new(200, 0, 200, 0))
            .expect("fits");
        assert!(close(n.position, [50.0, 0.0]));
        assert!(close(n.size, [100.0, 100.0]));
    }
}
