//! Where a dialog window opens: centred on the window it belongs to (U-rule: a
//! window opened from RBS appears over RBS, not where the OS happens to put it).

use slint::{PhysicalPosition, PhysicalSize};

/// The top-left corner that centres a `child` window of this size on `parent`.
/// Sizes and positions are physical pixels, as Slint reports them. The corner is
/// kept at or right of and below the parent's top-left corner, so a child larger
/// than its parent never starts above or left of it (its title bar stays reachable).
pub fn centred(
    parent: PhysicalPosition,
    parent_size: PhysicalSize,
    child_size: PhysicalSize,
) -> PhysicalPosition {
    let axis = |at: i32, outer: u32, inner: u32| {
        let free = i64::from(outer) - i64::from(inner);
        let offset = (free / 2).max(0);
        let at = i64::from(at) + offset;
        i32::try_from(at).unwrap_or(i32::MAX)
    };
    PhysicalPosition::new(
        axis(parent.x, parent_size.width, child_size.width),
        axis(parent.y, parent_size.height, child_size.height),
    )
}

/// Moves `child` to the centre of `parent` (see [`centred`]). `size` is the size the
/// child opens at, in logical pixels: right after `show` the OS may not have sized
/// the window yet, so its preferred size is passed instead of read back.
pub fn centre_on(child: &slint::Window, parent: &slint::Window, size: slint::LogicalSize) {
    let size = size.to_physical(child.scale_factor());
    if size.width == 0 || size.height == 0 {
        return;
    }
    child.set_position(centred(parent.position(), parent.size(), size));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn centres_and_keeps_the_title_bar_reachable() {
        let p = PhysicalPosition::new(100, 50);
        let at = centred(
            p,
            PhysicalSize::new(1400, 900),
            PhysicalSize::new(1000, 700),
        );
        assert_eq!(at, PhysicalPosition::new(300, 150));
        // A child larger than its parent starts at the parent's corner.
        let at = centred(p, PhysicalSize::new(800, 600), PhysicalSize::new(1000, 700));
        assert_eq!(at, p);
        // Negative positions (a monitor left of the primary one) work too.
        let at = centred(
            PhysicalPosition::new(-1920, 0),
            PhysicalSize::new(1920, 1080),
            PhysicalSize::new(1000, 800),
        );
        assert_eq!(at, PhysicalPosition::new(-1460, 140));
    }
}
