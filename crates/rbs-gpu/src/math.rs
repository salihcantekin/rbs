//! Small 2D math types for layer placement.

/// A 2D affine transform.
///
/// Maps a point `(x, y)` to `(a*x + c*y + tx, b*x + d*y + ty)`. For layers it maps
/// the unit square `(0,0)-(1,1)` of the source to canvas pixels, origin top left.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Affine2 {
    /// Column 0, x component.
    pub a: f32,
    /// Column 0, y component.
    pub b: f32,
    /// Column 1, x component.
    pub c: f32,
    /// Column 1, y component.
    pub d: f32,
    /// Translation x.
    pub tx: f32,
    /// Translation y.
    pub ty: f32,
}

impl Affine2 {
    /// The identity transform.
    pub const IDENTITY: Self = Self {
        a: 1.0,
        b: 0.0,
        c: 0.0,
        d: 1.0,
        tx: 0.0,
        ty: 0.0,
    };

    /// Maps the unit square to the axis-aligned rectangle at `(x, y)` with size `w x h`.
    pub const fn rect(x: f32, y: f32, w: f32, h: f32) -> Self {
        Self {
            a: w,
            b: 0.0,
            c: 0.0,
            d: h,
            tx: x,
            ty: y,
        }
    }

    /// A rotation by `radians` (clockwise on screen, since y points down) about the origin.
    pub fn rotation(radians: f32) -> Self {
        let (s, c) = radians.sin_cos();
        Self {
            a: c,
            b: s,
            c: -s,
            d: c,
            tx: 0.0,
            ty: 0.0,
        }
    }

    /// A translation.
    pub const fn translation(x: f32, y: f32) -> Self {
        Self {
            a: 1.0,
            b: 0.0,
            c: 0.0,
            d: 1.0,
            tx: x,
            ty: y,
        }
    }

    /// The transform `t` of the way from `self` to `to` (`t` is 0 at `self`, 1 at `to`).
    pub fn lerp(self, to: Self, t: f32) -> Self {
        let m = |a: f32, b: f32| a + (b - a) * t;
        Self {
            a: m(self.a, to.a),
            b: m(self.b, to.b),
            c: m(self.c, to.c),
            d: m(self.d, to.d),
            tx: m(self.tx, to.tx),
            ty: m(self.ty, to.ty),
        }
    }

    /// Returns the transform that applies `self` first, then `next`.
    pub fn then(&self, next: &Self) -> Self {
        Self {
            a: next.a * self.a + next.c * self.b,
            b: next.b * self.a + next.d * self.b,
            c: next.a * self.c + next.c * self.d,
            d: next.b * self.c + next.d * self.d,
            tx: next.a * self.tx + next.c * self.ty + next.tx,
            ty: next.b * self.tx + next.d * self.ty + next.ty,
        }
    }

    /// Applies the transform to a point.
    pub fn apply(&self, x: f32, y: f32) -> (f32, f32) {
        (
            self.a * x + self.c * y + self.tx,
            self.b * x + self.d * y + self.ty,
        )
    }
}

impl Default for Affine2 {
    fn default() -> Self {
        Self::IDENTITY
    }
}

/// A sub-rectangle of a texture in normalised coordinates, origin top left.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct UvRect {
    /// Left edge, 0.0..=1.0.
    pub u0: f32,
    /// Top edge, 0.0..=1.0.
    pub v0: f32,
    /// Right edge, 0.0..=1.0.
    pub u1: f32,
    /// Bottom edge, 0.0..=1.0.
    pub v1: f32,
}

impl UvRect {
    /// The whole texture.
    pub const FULL: Self = Self {
        u0: 0.0,
        v0: 0.0,
        u1: 1.0,
        v1: 1.0,
    };

    /// A crop given in source pixels.
    pub fn from_pixels(x: u32, y: u32, w: u32, h: u32, tex_w: u32, tex_h: u32) -> Self {
        let tw = tex_w.max(1) as f32;
        let th = tex_h.max(1) as f32;
        Self {
            u0: x as f32 / tw,
            v0: y as f32 / th,
            u1: (x + w) as f32 / tw,
            v1: (y + h) as f32 / th,
        }
    }
}

impl Default for UvRect {
    fn default() -> Self {
        Self::FULL
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: (f32, f32), b: (f32, f32)) -> bool {
        (a.0 - b.0).abs() < 1e-4 && (a.1 - b.1).abs() < 1e-4
    }

    #[test]
    fn rect_maps_unit_square_corners() {
        let t = Affine2::rect(10.0, 20.0, 100.0, 50.0);
        assert!(close(t.apply(0.0, 0.0), (10.0, 20.0)));
        assert!(close(t.apply(1.0, 1.0), (110.0, 70.0)));
    }

    #[test]
    fn then_applies_self_first() {
        let scale = Affine2::rect(0.0, 0.0, 2.0, 2.0);
        let shift = Affine2::translation(5.0, 0.0);
        let t = scale.then(&shift);
        assert!(close(t.apply(1.0, 1.0), (7.0, 2.0)));
        let t = shift.then(&scale);
        assert!(close(t.apply(1.0, 1.0), (12.0, 2.0)));
    }

    #[test]
    fn rotation_quarter_turn() {
        let t = Affine2::rotation(std::f32::consts::FRAC_PI_2);
        assert!(close(t.apply(1.0, 0.0), (0.0, 1.0)));
    }

    #[test]
    fn uv_from_pixels() {
        let r = UvRect::from_pixels(50, 0, 50, 100, 100, 100);
        assert_eq!(
            r,
            UvRect {
                u0: 0.5,
                v0: 0.0,
                u1: 1.0,
                v1: 1.0
            }
        );
    }
}
