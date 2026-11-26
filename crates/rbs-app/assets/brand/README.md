# RBS brand assets

- `rbs-mark.svg`: the 6-blade hexagonal aperture mark alone (dark-theme colour `#8A63FF`). In the app it is drawn by `BrandMark` in `ui/components.slint` with theme token `color.brand`.
- `rbs-icon.svg`: the application icon master: an anodized RBS violet 6-blade hexagonal aperture lit softly from the top-left on a dark obsidian tile with a thin metallic rim. Fixed colours, the OS does not follow the theme.
- `icon-win-256.png`, `rbs.ico`: Windows (tile fills the canvas; `.ico` includes 16, 24, 32, 48, 64, 128 and 256 px). `icon-mac-1024.png`, `icon-mac-256.png`, `rbs.icns`: macOS (tile with the system margin).
- `favicon-32.png`, `favicon-16.png`: web favicons (full-bleed tile with wider seam spacing at small sizes).

`generate_icons.py` draws `rbs-icon.svg`, `rbs-mark.svg`, the rasters, `rbs.ico` and `rbs.icns` from one design description (needs Pillow and macOS `iconutil`; run it from this directory after any change to the design). `BrandMark` in `ui/components.slint` embeds `rbs-mark.svg` at compile time and tints it with `Theme.brand`.

## Where the icons are used

- macOS: `crates/rbs-app/macos/bundle.sh` builds `RBS.app` (name "RBS", `rbs.icns`) from the built executable; the app also sets the Dock icon and the app name at run time so a bare `cargo run` shows the logo. It signs the bundle with the local identity "RBS Development" when the keychain has it (`crates/rbs-app/macos/dev-identity.sh` creates it once per Mac), so macOS privacy grants survive rebuilds (ADR-0141 follow-up); otherwise ad hoc.
- Windows: `crates/rbs-app/build.rs` embeds `rbs.ico` as the executable's icon resource (file and taskbar icon).
