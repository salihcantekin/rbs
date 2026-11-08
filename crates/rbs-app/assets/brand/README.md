# RBS brand assets

- `rbs-mark.svg`: the mark alone (dark-theme colours). In the app it is drawn by `BrandMark` in `ui/components.slint` with theme tokens (`color.text.primary`, `color.brand`).
- `rbs-icon.svg`: the application icon master (fixed colours: tile `#1C2136`, frame `#E6E9FF`, dot `#7C52FF`).
- `icon-win-256.png`, `rbs.ico`: Windows (tile fills the canvas). `icon-mac-1024.png`, `icon-mac-256.png`, `rbs.icns`: macOS (tile with the system margin).

The rasters are drawn from the same geometry as the SVGs (rounded square 4..60, radius 14; bite circle at 46,46 radius 17; dot radius 11). Regenerate them after any change to the mark.
