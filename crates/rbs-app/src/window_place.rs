//! Where a dialog window opens: centred on the window it belongs to (U-rule: a
//! window opened from RBS appears over RBS, not where the OS happens to put it).
//! Also brings an already-open window to the front when the user asks for it again.

use slint::{
    ComponentHandle, LogicalPosition, LogicalSize, PhysicalPosition, PhysicalSize, PlatformError,
};

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

/// Logical top-left that centres a child of `child` points on a parent whose
/// position and size are physical pixels at `scale`.
///
/// Slint's `Window::position` and `Window::size` are physical pixels (the window's
/// point size times its scale). A new window still reports scale 1, the value Slint
/// stores until the screen's scale arrives, so the child size must be converted with
/// the parent's scale, the window it is placed on, already on screen. The result is
/// in points. `set_position` of a physical point is divided by the screen scale on
/// the way to AppKit; a logical point is not, so the same scale is not applied twice.
fn centred_logical(
    parent: PhysicalPosition,
    parent_size: PhysicalSize,
    child: LogicalSize,
    scale: f32,
) -> LogicalPosition {
    centred(parent, parent_size, child.to_physical(scale)).to_logical(scale)
}

/// The logical size to centre with. `reported` is `Window::size` (physical pixels)
/// and `child_scale` is what the child currently reports.
///
/// Until the child is on a screen its scale stays 1 while `reported` may already be
/// the logical size stored in the physical slot, or the scale may already be the
/// screen's while `reported` has not been updated yet. Either mix is smaller than
/// the size the window was asked to open at, and centring with it shifts the window
/// by a quarter of that size on a 2x display. The requested size is used then.
fn child_logical_size(
    requested: LogicalSize,
    reported: PhysicalSize,
    child_scale: f32,
    parent_scale: f32,
) -> LogicalSize {
    let scales_match = child_scale.is_finite()
        && parent_scale.is_finite()
        && (child_scale - parent_scale).abs() < 0.05;
    if !scales_match || reported.width == 0 || reported.height == 0 {
        return requested;
    }
    let logical = reported.to_logical(parent_scale);
    if logical.width + 1.0 < requested.width || logical.height + 1.0 < requested.height {
        requested
    } else {
        logical
    }
}

/// Moves `child` to the centre of `parent` (see [`centred`]). `size` is the size the
/// child opens at, in logical pixels: before the OS has sized the window its
/// preferred size is passed instead of read back.
pub fn centre_on(child: &slint::Window, parent: &slint::Window, size: LogicalSize) {
    let scale = parent.scale_factor();
    if !scale.is_finite() || scale <= 0.0 || size.width <= 0.0 || size.height <= 0.0 {
        return;
    }
    child.set_position(centred_logical(
        parent.position(),
        parent.size(),
        size,
        scale,
    ));
}

/// Shows `child` centred on `parent`, the same way on every OS. This is the one way a
/// dialog window of RBS is shown the first time (a test keeps it so).
///
/// The position is set before `show`, so the window is created where it belongs and
/// no OS (macOS cascades new windows) places it first. Once the window is up, one
/// more pass at the next turn of the event loop centres it again, since macOS
/// applies the creation position to the content origin and may place the window
/// again after `show` returns.
///
/// `parent_hidden` is true while every window is hidden (ADR-0056): there is
/// nothing to centre on, and the OS places the window.
///
/// When the window is already open and only needs to come forward, use [`raise`]
/// instead so the user's place is kept.
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
    #[cfg(target_os = "macos")]
    forget_saved_frame(child.window());
    centre_on(child.window(), parent.window(), size);
    let (child, parent) = (child.as_weak(), parent.as_weak());
    // The window is created on a later turn of the event loop, and macOS reads the
    // position stored before that as the content origin (the title bar sits above
    // it). One more pass, once the window exists, sets the frame origin, and a
    // second pass covers the same placement being applied again when the window
    // becomes key. Both use the size the window was asked for, not a size read
    // back before the screen scale and the physical size agree.
    //
    // The two passes are scheduled from here, not nested inside each other: a
    // zero-delay timer that starts another zero-delay timer from its body can
    // re-enter Slint's timer activation through AppKit while Settings opens from
    // the native menu bar, and that abort cannot unwind (ADR-0055).
    let requested = size;
    let again = (child.clone(), parent.clone());
    crate::entry::single_shot(std::time::Duration::ZERO, move || {
        centre_later(child, parent, requested);
    });
    crate::entry::single_shot(std::time::Duration::from_millis(16), move || {
        centre_later(again.0, again.1, requested);
    });
    Ok(())
}

/// Brings an already-shown window in front of the others of this process, without
/// moving it. Restores it when minimised. No-op when the OS handle is not ready yet
/// (Linux today has no extra call: `show` alone does not raise an obscured window).
pub fn raise(window: &slint::Window) {
    // `RawWindowHandle` names only the Windows and macOS arms below.
    #[cfg_attr(not(any(windows, target_os = "macos")), allow(unused_imports))]
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    // `window_handle()` returns a guard; keep it for the borrow of the raw handle.
    let binding = window.window_handle();
    let Ok(handle) = binding.window_handle() else {
        return;
    };
    match handle.as_raw() {
        #[cfg(windows)]
        RawWindowHandle::Win32(win) => rbs_app::win::bring_forward(win.hwnd),
        #[cfg(target_os = "macos")]
        RawWindowHandle::AppKit(view) => {
            if let Some(view) = std::ptr::NonNull::new(view.ns_view.as_ptr()) {
                rbs_app::native::bring_forward(view);
            }
        }
        _ => {}
    }
}

fn centre_later<C: ComponentHandle, P: ComponentHandle>(
    child: slint::Weak<C>,
    parent: slint::Weak<P>,
    requested: LogicalSize,
) {
    if let (Some(child), Some(parent)) = (child.upgrade(), parent.upgrade()) {
        #[cfg(target_os = "macos")]
        forget_saved_frame(child.window());
        let size = child_logical_size(
            requested,
            child.window().size(),
            child.window().scale_factor(),
            parent.window().scale_factor(),
        );
        centre_on(child.window(), parent.window(), size);
    }
}

/// Clears macOS's remembered frame for this window, so centring is not undone by it.
#[cfg(target_os = "macos")]
fn forget_saved_frame(window: &slint::Window) {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    let handle = window.window_handle();
    let Ok(handle) = handle.window_handle() else {
        return;
    };
    if let RawWindowHandle::AppKit(view) = handle.as_raw()
        && let Some(view) = std::ptr::NonNull::new(view.ns_view.as_ptr())
    {
        rbs_app::native::forget_saved_frame(view);
    }
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

    /// Scale 2, parent not at the origin, child given as a logical size.
    ///
    /// `Window::position` and `Window::size` are physical pixels. A Settings window
    /// that has not yet been on screen still reports scale 1, so converting its
    /// logical size with that factor leaves it in points. `set_position` of the
    /// resulting physical point is then divided by the screen scale (2 on this
    /// Mac). The window opens a quarter of its own size to the right and down of
    /// the parent's centre: for 920 x 680 points, 230 and 170.
    #[test]
    fn logical_child_is_centred_at_the_parent_scale() {
        let parent = PhysicalPosition::new(200, 100);
        let parent_size = PhysicalSize::new(2880, 1800);
        let child = LogicalSize::new(920.0, 680.0);
        let at = centred_logical(parent, parent_size, child, 2.0);
        assert_eq!(at, LogicalPosition::new(360.0, 160.0));

        let unscaled = centred(parent, parent_size, child.to_physical(1.0)).to_logical(2.0);
        assert_eq!(
            unscaled,
            LogicalPosition::new(360.0 + 920.0 / 4.0, 160.0 + 680.0 / 4.0)
        );
        assert_ne!(unscaled, at);
    }

    /// A size read back before the child's scale and its physical size agree is
    /// not used. Half of 920 x 680 is what scale 2 makes of a logical size still
    /// sitting in the physical slot.
    #[test]
    fn stale_child_size_keeps_the_requested_logical_size() {
        let requested = LogicalSize::new(920.0, 680.0);
        let stale = child_logical_size(requested, PhysicalSize::new(920, 680), 2.0, 2.0);
        assert_eq!(stale, requested);
        let real = child_logical_size(requested, PhysicalSize::new(1840, 1360), 2.0, 2.0);
        assert_eq!(real, requested);
        let grown = child_logical_size(requested, PhysicalSize::new(2000, 1500), 2.0, 2.0);
        assert_eq!(grown, LogicalSize::new(1000.0, 750.0));
        let not_yet = child_logical_size(requested, PhysicalSize::new(0, 0), 1.0, 2.0);
        assert_eq!(not_yet, requested);
    }

    /// A dialog's first open goes through `show_centred` (over the main window on every
    /// OS). An already-open dialog is brought forward with `raise`, not re-centred.
    /// Centring after a plain `show()` is what left Settings off-centre on macOS.
    #[test]
    fn dialog_windows_are_shown_only_through_show_centred() {
        let src = include_str!("main.rs");
        assert!(
            src.contains("window_place::show_centred("),
            "Settings must be shown with show_centred"
        );
        assert!(
            src.contains("window_place::raise("),
            "an open dialog is raised, not shown again"
        );
        assert!(
            !src.contains("window_place::centre_on("),
            "use window_place::show_centred, not centre_on after show"
        );
        // Settings and the source settings window are not shown with a bare `show()`.
        assert!(
            !src.contains("let _ = w.show();"),
            "dialog windows are shown with show_centred"
        );
        assert!(
            src.contains("fn present_settings"),
            "Settings has one presenter"
        );
    }
}
