#!/bin/sh
# Builds the RBS app and the programs it starts from its own folder, and puts them
# together so the app finds them (macOS and Linux; Windows: build.ps1).
#
#   tools/dev/build.sh [--release] [--no-browser] [--browser-only] [--test-browser]
#
#   --release       optimised build (target/release); default is debug (target/debug)
#   --no-browser    skip the CEF browser runner (browser sources stay Unavailable)
#   --browser-only  build only the CEF browser runner
#   --test-browser  also build rbs-browser-helper, the synthetic page used without CEF
#
# What it puts next to `rbs` (rbs-app `browser_tree::resolve_runner`, `ai_tools`):
#   rbs-mcp             AI tools server
#   rbs-browser-cef     CEF browser runner; on macOS the bundle rbs-browser-cef.app
#                       with its helper apps, inside RBS.app/Contents/MacOS
# The browser engine (CEF runtime) is not built here: the app downloads it in
# Settings > Add-ons (ADR-0145). The runner build needs cmake, ninja and a C++
# compiler; the first one downloads CEF once for its headers and import library.
# Honours CARGO_TARGET_DIR and CARGO_BUILD_JOBS.
set -eu
root=$(cd "$(dirname "$0")/../.." && pwd)
target=${CARGO_TARGET_DIR:-$root/target}
profile=debug
release_flag=
app=1
browser=1
test_browser=0
for arg in "$@"; do
    case $arg in
        --release) profile=release; release_flag=--release ;;
        --no-browser) browser=0 ;;
        --browser-only) app=0 ;;
        --test-browser) test_browser=1 ;;
        -h|--help) sed -n '2,19p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) echo "build.sh: unknown option $arg (see --help)" >&2; exit 2 ;;
    esac
done
out=$target/$profile
os=$(uname -s)

step() { printf '\n== %s\n' "$*"; }

if [ "$app" = 1 ]; then
    step "Building rbs and rbs-mcp ($profile)"
    set -- -p rbs-app -p rbs-mcp
    if [ "$test_browser" = 1 ]; then set -- "$@" -p rbs-browser-runner; fi
    (cd "$root" && cargo build $release_flag "$@")
fi

if [ "$browser" = 1 ]; then
    step "Building the CEF browser runner ($profile)"
    # Its own workspace and lock file (ADR-0132 decision 1); the shared target
    # directory puts the executables beside `rbs`.
    cargo build $release_flag --manifest-path "$root/crates/rbs-browser-cef/Cargo.toml" \
        --target-dir "$target"
fi

if [ "$os" = Darwin ]; then
    # shellcheck source=tools/dev/macos-bundle.sh
    . "$root/tools/dev/macos-bundle.sh"
    if [ "$app" = 1 ]; then
        step "Making RBS.app"
        "$root/crates/rbs-app/macos/bundle.sh" "$profile" "$out" >/dev/null
        cp "$out/rbs-mcp" "$out/RBS.app/Contents/MacOS/rbs-mcp"
    fi
    if [ "$browser" = 1 ] && [ -d "$out/RBS.app" ]; then
        step "Putting rbs-browser-cef.app into RBS.app"
        runner_bundle "$out" "$out/RBS.app/Contents/MacOS"
    fi
    if [ -d "$out/RBS.app" ]; then
        sign_app "$out/RBS.app"
        printf '\nReady: %s\n' "$out/RBS.app"
    fi
else
    printf '\nReady: %s\n' "$out/rbs"
fi
if [ "$browser" = 0 ] && [ "$app" = 1 ]; then
    echo "No browser runner built: browser sources show helper_missing."
fi
