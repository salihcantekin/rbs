//! Where a dialog window opens: centred on the window it belongs to (U-rule: a
//! window opened from RBS appears over RBS, not where the OS happens to put it).

use slint::{ComponentHandle, LogicalSize, PhysicalPosition, PhysicalSize, PlatformError};

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
/// child opens at, in logical pixels: before the OS has sized the window its
/// preferred size is passed instead of read back.
pub fn centre_on(child: &slint::Window, parent: &slint::Window, size: LogicalSize) {
    let size = size.to_physical(child.scale_factor());
    if size.width == 0 || size.height == 0 {
        return;
    }
    child.set_position(centred(parent.position(), parent.size(), size));
}

/// Shows `child` centred on `parent`, the same way on every OS. This is the one way a
/// dialog window of RBS is shown (a test keeps it so).
///
/// The position is set before `show`, so the window is created where it belongs and
/// no OS (macOS cascades new windows) places it first. Once the window is up, one
/// more pass at the next turn of the event loop centres it again on the size it
/// really got, since some window systems apply the size or their own placement
/// after `show` returns.
///
/// `parent_hidden` is true while every window is hidden (ADR-0056): there is
/// nothing to centre on, and the OS places the window.
pub fn show_centred<C: ComponentHandle + 'static, P: ComponentHandle + 'static>(
    child: &C,
    parent: &P,
    size: LogicalSize,
    parent_hidden: bool,
) -> Result<(), PlatformError> {
    if parent_hidden {
        return child.show();
    }
    centre_on(child.window(), parent.window(), size);
    child.show()?;
    centre_on(child.window(), parent.window(), size);
    let (child, parent) = (child.as_weak(), parent.as_weak());
    crate::entry::single_shot(std::time::Duration::ZERO, move || {
        if let (Some(child), Some(parent)) = (child.upgrade(), parent.upgrade()) {
            // The size the window really has now, not the preferred one.
            let real = child
                .window()
                .size()
                .to_logical(child.window().scale_factor());
            centre_on(child.window(), parent.window(), real);
        }
    });
    Ok(())
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

    /// Every dialog window of RBS is shown by `show_centred`, so it opens over the
    /// main window on every OS. Centring after a plain `show()` is what left Settings
    /// off-centre on macOS while it worked on Windows.
    #[test]
    fn dialog_windows_are_shown_only_through_show_centred() {
        let src = include_str!("main.rs");
        assert!(
            src.contains("window_place::show_centred("),
            "Settings must be shown with show_centred"
        );
        assert!(
            !src.contains("window_place::centre_on("),
            "use window_place::show_centred, not centre_on after show"
        );
        // `present_settings` is the one place that shows the Settings window.
        let shows = src.matches("let _ = w.show();").count();
        assert_eq!(shows, 1, "Settings is shown only in `present_settings`");
    }
}
