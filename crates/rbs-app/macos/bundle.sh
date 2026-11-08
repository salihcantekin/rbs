#!/bin/sh
# Builds RBS.app from an rbs-app executable, so macOS shows the name "RBS" and the
# RBS icon in the Dock, Cmd+Tab and the menu bar.
#   crates/rbs-app/macos/bundle.sh [profile] [output-dir]
# profile: debug (default) or release; the executable must already be built.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../../.." && pwd)
profile=${1:-debug}
out=${2:-$root/target/$profile}
version=$(sed -n 's/^version = "\(.*\)"/\1/p' "$root/Cargo.toml" | head -1)
app="$out/RBS.app"
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp "$root/target/$profile/rbs" "$app/Contents/MacOS/rbs"
cp "$here/../assets/brand/rbs.icns" "$app/Contents/Resources/rbs.icns"
sed "s/@VERSION@/${version:-0.0.0}/g" "$here/Info.plist" > "$app/Contents/Info.plist"
echo "$app"
