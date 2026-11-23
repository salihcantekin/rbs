#!/bin/sh
# Makes the download zip of RBS for this computer (macOS and Linux; Windows:
# package.ps1). The same script runs on a developer machine and in CI.
#
#   tools/package/package.sh [--no-build] [--label TEXT] [--out DIR]
#
#   --no-build   package what is already built in target/release
#   --label TEXT name part after the version (default: the platform, such as macos-arm64)
#   --out DIR    where the zip goes (default: dist/ in the repository)
#
# Builds the release app with tools/dev/build.sh --release (rbs, rbs-mcp and the CEF
# browser runner), collects exactly the files a user needs into
# <out>/RBS-<version>-<label>/ and zips it to <out>/RBS-<version>-<label>.zip, with a
# .sha256 file beside it. The browser engine (CEF runtime) is not included: the app
# downloads it in Settings > Add-ons (ADR-0145). Prints the zip path on its last line.
# Honours CARGO_TARGET_DIR and CARGO_BUILD_JOBS.
set -eu
root=$(cd "$(dirname "$0")/../.." && pwd)
target=${CARGO_TARGET_DIR:-$root/target}
out=$root/dist
build=1
label=
while [ $# -gt 0 ]; do
    case $1 in
        --no-build) build=0 ;;
        --label) shift; label=${1:?--label needs a value} ;;
        --out) shift; out=${1:?--out needs a value} ;;
        -h|--help) sed -n '2,17p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) echo "package.sh: unknown option $1 (see --help)" >&2; exit 2 ;;
    esac
    shift
done
os=$(uname -s)
arch=$(uname -m)
case $os in
    Darwin) platform=macos ;;
    Linux) platform=linux ;;
    *) echo "package.sh: unsupported system $os (Windows: package.ps1)" >&2; exit 2 ;;
esac
case $arch in arm64|aarch64) arch=arm64 ;; x86_64|amd64) arch=x64 ;; esac
version=$(sed -n 's/^version = "\(.*\)"/\1/p' "$root/Cargo.toml" | head -1)
[ -n "$version" ] || { echo "package.sh: no version in Cargo.toml" >&2; exit 1; }
label=${label:-$platform-$arch}
name=RBS-$version-$label
bin=$target/release

if [ "$build" = 1 ]; then
    "$root/tools/dev/build.sh" --release
fi

stage=$out/$name
rm -rf "$stage" "$out/$name.zip" "$out/$name.zip.sha256"
mkdir -p "$stage"

need() { [ -e "$1" ] || { echo "package.sh: missing $1 (build first, or drop --no-build)" >&2; exit 1; }; }

if [ "$platform" = macos ]; then
    need "$bin/RBS.app"
    # ditto keeps the signature, symbolic links and extended attributes.
    ditto "$bin/RBS.app" "$stage/RBS.app"
    run='Open the RBS folder and double-click RBS.app. The first time, macOS may say it cannot
verify the app: right-click RBS.app, choose Open, then Open again.'
    data='~/Library/Application Support/RBS'
else
    need "$bin/rbs"
    need "$bin/rbs-mcp"
    cp "$bin/rbs" "$bin/rbs-mcp" "$stage/"
    # The browser runner is optional: without it browser sources are unavailable.
    if [ -e "$bin/rbs-browser-cef" ]; then cp "$bin/rbs-browser-cef" "$stage/"; fi
    run='Run ./rbs from this folder.'
    data='~/.config/rbs'
fi

cp "$root/LICENSE" "$stage/LICENSE"
awk -v run="$run" -v data="$data" '{ gsub(/@RUN@/, run); gsub(/@DATA@/, data); print }' \
    "$root/tools/package/README-user.txt" > "$stage/README.txt"
printf '%s\n' "$version" > "$stage/VERSION.txt"

(
    cd "$out"
    if [ "$platform" = macos ]; then
        ditto -c -k --keepParent "$name" "$name.zip"
    else
        zip -qr -X "$name.zip" "$name"
    fi
    if command -v shasum >/dev/null 2>&1; then shasum -a 256 "$name.zip" > "$name.zip.sha256"
    else sha256sum "$name.zip" > "$name.zip.sha256"; fi
)
rm -rf "$stage"
printf '%s\n' "$out/$name.zip"
