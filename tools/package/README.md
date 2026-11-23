# Download packages

Scripts that make the zip users download and run (no installer yet). The same script
runs on a developer machine and in CI, so a release zip is exactly what you can make
locally.

| Platform | Command | Result |
| --- | --- | --- |
| Windows | `.\tools\package\package.ps1` | `dist\RBS-<version>-windows-x64.zip` |
| macOS, Linux | `tools/package/package.sh` | `dist/RBS-<version>-<platform>-<arch>.zip` |

Each run builds the release app with `tools/dev/build.*` (`rbs`, `rbs-mcp` and the CEF
browser runner), copies the files a user needs into a folder, adds `LICENSE`, `VERSION.txt` and
everything in `tools/package/<platform>/` (`windows`, `macos`, `linux`: the `README.txt`
and any other installation notes; edit them there, no script change needed), zips it and writes a `.sha256` beside the zip. The last
line printed is the zip path. Options: `--no-build` / `-NoBuild` (package what is
already built), `--label` / `-Label` (the name part after the version, for example a
tag), `--out` / `-Out` (the output folder).

Contents:

- Windows: `rbs.exe`, `rbs-mcp.exe`, `rbs-browser-cef.exe`.
- macOS: `RBS.app` (with `rbs-mcp` and `rbs-browser-cef.app` inside), zipped with `ditto` to keep the
  signature and links. A build without a Developer ID is signed with the development
  identity or ad hoc, so the user opens it once with right-click, Open.
- Linux: `rbs`, `rbs-mcp`, `rbs-browser-cef`.

Not in the zip: the browser engine (CEF runtime; the app downloads it in Settings ›
Add-ons, ADR-0145) and the Windows virtual camera (it needs a signed installation
under Program Files, ADR-0066 and ADR-0070).

Later, a CI job runs the script for each platform and attaches the zip and its `.sha256`
to a release.
