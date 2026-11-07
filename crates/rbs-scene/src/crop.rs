//! Crop edits that keep the picture's scale (ADR-0058).

use crate::flatten::Affine2;
use crate::model::{Crop, Layout};

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
    fn an_old_crop_larger_than_the_picture_keeps_the_centre() {
        let l = Layout::new([0.0, 0.0], [200.0, 100.0]).with_crop(Crop::new(500, 0, 500, 0));
        let n = l
            .recrop((800, 400), Crop::new(200, 0, 200, 0))
            .expect("fits");
        assert!(close(n.position, [50.0, 0.0]));
        assert!(close(n.size, [100.0, 100.0]));
    }
}
