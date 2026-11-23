# Build and run scripts

Scripts for building the RBS app together with the programs it starts from its own
folder, and for running it in different ways. `build.sh` and `run.sh` are for macOS
and Linux, `build.ps1` and `run.ps1` for Windows (PowerShell). Each prints its options
with `--help` (`Get-Help .\tools\dev\build.ps1 -Detailed` on Windows).

## Build

| Command | What it builds |
| --- | --- |
| `tools/dev/build.sh` | debug app, `rbs-mcp` and the CEF browser runner |
| `tools/dev/build.sh --release` | the same, optimised |
| `tools/dev/build.sh --no-browser` | without the browser runner |
| `tools/dev/build.sh --browser-only` | only the browser runner |
| `tools/dev/build.sh --test-browser` | also `rbs-browser-helper`, the synthetic page used without CEF |

Windows: `.\tools\dev\build.ps1` with `-Release`, `-NoBrowser`, `-BrowserOnly`,
`-TestBrowser`. It also builds the virtual camera (`rbs_vcam_mf_source.dll`).

The result is in `target/debug` or `target/release` (`$CARGO_TARGET_DIR` when set):

| Platform | Layout |
| --- | --- |
| Windows | `rbs.exe`, `rbs-mcp.exe`, `rbs_vcam_mf_source.dll` and `rbs-browser-cef.exe` side by side |
| Linux | `rbs`, `rbs-mcp` and `rbs-browser-cef` side by side |
| macOS | `RBS.app`, with `rbs-mcp` and `rbs-browser-cef.app` (and its five helper apps) in `RBS.app/Contents/MacOS`, signed with "RBS Development" when the keychain has it |

The browser runner is its own Cargo workspace (`crates/rbs-browser-cef`, ADR-0132
decision 1), built with `--target-dir` pointing at the app's target folder so it lands
next to `rbs`. It needs cmake, ninja and a C++ compiler; on Windows the script loads
the Visual Studio `vcvars64` environment itself when `rc.exe` is not on `PATH`. The
first build downloads CEF once for its headers. The browser engine itself (the CEF
runtime) is not part of the build: the app downloads it in Settings › Add-ons
(ADR-0145, [add-ons spec](../../docs/specs/add-ons.md)).

## Run

| Command | How it runs |
| --- | --- |
| `tools/dev/run.sh` | the debug build |
| `tools/dev/run.sh --release` | the release build |
| `tools/dev/run.sh --build` | builds first, then runs (add `--release`, `--no-browser`) |
| `tools/dev/run.sh --ui-dev` | UI live preview: saving a `.slint` file reloads the windows (`cargo xtask ui-dev`) |
| `tools/dev/run.sh --test-browser` | browser sources show the synthetic page instead of CEF (`RBS_BROWSER_HELPER`) |
| `tools/dev/run.sh --fresh-config` | starts from an empty configuration in a new temporary folder; yours is untouched |
| `tools/dev/run.sh --config DIR` | uses `DIR` as the configuration folder (`RBS_CONFIG_DIR`) |
| `tools/dev/run.sh --log LEVELS` | sets the diagnostics log levels for this run (`RBS_LOG`) |

Windows: `.\tools\dev\run.ps1` with `-Release`, `-Build`, `-UiDev`, `-TestBrowser`,
`-FreshConfig`, `-Config <dir>`, `-Log <levels>`. Arguments after `--` go to the app.

Examples:

```sh
tools/dev/run.sh --build --release          # build everything optimised and run it
tools/dev/run.sh --fresh-config             # try the first-run page
```

```powershell
.\tools\dev\run.ps1 -Build -Release
.\tools\dev\run.ps1 -FreshConfig
```

These scripts are for development. Measurements use `cargo xtask device-check app`
([testing strategy](../../docs/specs/testing-strategy.md)), and no package or installer
is made here.
