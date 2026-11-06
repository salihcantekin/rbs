//! ADR-0056 decision 9: on Windows the tray menu's events reach RBS through `muda`'s one
//! process-wide event handler, which `rbs_app::win::install_handlers` sets at start.
//! Slint's winit backend sets the same handler for a native menu bar or context menu,
//! and only the first setter wins. Today RBS has neither in its UI; when one is added,
//! the tray handler must first pass Slint's events on, or Slint's menus stop working on
//! Windows. This test fails until then.

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

#[test]
fn no_native_slint_menus_while_the_tray_owns_menu_events() {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/ui");
    for entry in std::fs::read_dir(dir).expect("the ui directory") {
        let path = entry.unwrap().path();
        if path.extension().is_none_or(|e| e != "slint") {
            continue;
        }
        let text = std::fs::read_to_string(&path).unwrap();
        for word in ["MenuBar", "ContextMenuArea"] {
            assert!(
                !text.contains(word),
                "{} uses {word}: rework rbs_app::win::install_handlers (ADR-0056 decision 9)",
                path.display()
            );
        }
    }
}
