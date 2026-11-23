#!/bin/sh
# Runs the RBS app in one of several ways (macOS and Linux; Windows: run.ps1).
#
#   tools/dev/run.sh [mode] [options] [-- app arguments]
#
# Modes (one):
#   (none)          the debug build
#   --release       the release build
#   --ui-dev        UI live preview: saving a .slint file reloads the windows, no Rust
#                   build (`cargo xtask ui-dev`; never for measurements)
#
# Options:
#   --build         build first (tools/dev/build.sh with the same profile)
#   --no-browser    with --build: skip the CEF browser runner
#   --test-browser  browser sources use rbs-browser-helper, the synthetic page, instead
#                   of CEF (built with --build; RBS_BROWSER_HELPER)
#   --fresh-config  start from an empty configuration in a new temporary folder (the
#                   first-run page); your own configuration is not touched
#   --config DIR    use DIR as the configuration folder (RBS_CONFIG_DIR)
#   --log LEVELS    diagnostics log levels for this run (RBS_LOG, docs/specs/diagnostics-log.md)
#
# On macOS the app runs from RBS.app/Contents/MacOS/rbs so it keeps the bundle's name,
# icon and privacy grants, and the log stays in this terminal.
set -eu
root=$(cd "$(dirname "$0")/../.." && pwd)
target=${CARGO_TARGET_DIR:-$root/target}
profile=debug
mode=build
build=0
build_flags=
test_browser=0
fresh=0
config=
log=
while [ $# -gt 0 ]; do
    case $1 in
        --release) profile=release; build_flags="$build_flags --release" ;;
        --ui-dev) mode=ui-dev ;;
        --build) build=1 ;;
        --no-browser) build_flags="$build_flags --no-browser" ;;
        --test-browser) test_browser=1; build_flags="$build_flags --test-browser" ;;
        --fresh-config) fresh=1 ;;
        --config) shift; config=${1:?--config needs a folder} ;;
        --log) shift; log=${1:?--log needs a level} ;;
        --) shift; break ;;
        -h|--help) sed -n '2,23p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) echo "run.sh: unknown option $1 (see --help)" >&2; exit 2 ;;
    esac
    shift
done
out=$target/$profile

if [ "$fresh" = 1 ]; then
    config=$(mktemp -d "${TMPDIR:-/tmp}/rbs-fresh-config.XXXXXX")
    echo "Fresh configuration: $config"
fi
if [ -n "$config" ]; then export RBS_CONFIG_DIR="$config"; fi
if [ -n "$log" ]; then export RBS_LOG="$log"; fi

if [ "$mode" = ui-dev ]; then
    if [ "$test_browser" = 1 ]; then
        echo "run.sh: --test-browser is not used with --ui-dev" >&2
    fi
    cd "$root"
    exec cargo xtask ui-dev "$@"
fi

if [ "$build" = 1 ]; then
    # shellcheck disable=SC2086 # the flags are words
    "$root/tools/dev/build.sh" $build_flags
fi

if [ "$test_browser" = 1 ]; then
    if [ ! -x "$out/rbs-browser-helper" ]; then
        echo "run.sh: $out/rbs-browser-helper is missing; add --build" >&2
        exit 1
    fi
    export RBS_BROWSER_HELPER="$out/rbs-browser-helper"
fi

if [ "$(uname -s)" = Darwin ] && [ -x "$out/RBS.app/Contents/MacOS/rbs" ]; then
    exe=$out/RBS.app/Contents/MacOS/rbs
else
    exe=$out/rbs
fi
if [ ! -x "$exe" ]; then
    echo "run.sh: $exe is missing; add --build" >&2
    exit 1
fi
exec "$exe" "$@"
