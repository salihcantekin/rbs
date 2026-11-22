# RBS brand assets

- `rbs-mark.svg`: the mark alone (dark-theme colours). In the app it is drawn by `BrandMark` in `ui/components.slint` with theme tokens (`color.text.primary`, `color.brand`).
- `rbs-icon.svg`: the application icon master: a window (title bar, sidebar, text lines) on a black tile with a thin rim and a camera bubble with a pink-to-violet ring in its lower right corner. Fixed colours, the OS does not follow the theme.
- `icon-win-256.png`, `rbs.ico`: Windows (tile fills the canvas). `icon-mac-1024.png`, `icon-mac-256.png`, `rbs.icns`: macOS (tile with the system margin).

`generate_icons.py` draws `rbs-icon.svg`, the rasters, `rbs.ico` and `rbs.icns` from one design description (needs Pillow and macOS `iconutil`; run it from this directory after any change to the design). `BrandMark` in the app has the same geometry, drawn in theme colours; keep them in step.

## Where the icons are used

- macOS: `crates/rbs-app/macos/bundle.sh` builds `RBS.app` (name "RBS", `rbs.icns`) from the built executable; the app also sets the Dock icon and the app name at run time so a bare `cargo run` shows the logo. It signs the bundle with the local identity "RBS Development" when the keychain has it (`crates/rbs-app/macos/dev-identity.sh` creates it once per Mac), so macOS privacy grants survive rebuilds (ADR-0141 follow-up); otherwise ad hoc.
- Windows: `crates/rbs-app/build.rs` embeds `rbs.ico` as the executable's icon resource (file and taskbar icon).
