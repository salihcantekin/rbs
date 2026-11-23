#!/bin/sh
# Builds RBS.app from an rbs-app executable, so macOS shows the name "RBS" and the
# RBS icon in the Dock, Cmd+Tab and the menu bar.
#   crates/rbs-app/macos/bundle.sh [profile] [output-dir]
# profile: debug (default) or release; the executable must already be built (in
# $CARGO_TARGET_DIR when it is set, else target/).
# Signs the bundle with the identity "RBS Development" (or $RBS_SIGN_IDENTITY) when
# the keychain has it, so privacy grants survive rebuilds (dev-identity.sh makes it
# once); otherwise ad hoc, and macOS may ask again after each rebuild.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../../.." && pwd)
profile=${1:-debug}
target=${CARGO_TARGET_DIR:-$root/target}
out=${2:-$target/$profile}
version=$(sed -n 's/^version = "\(.*\)"/\1/p' "$root/Cargo.toml" | head -1)
app="$out/RBS.app"
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp "$target/$profile/rbs" "$app/Contents/MacOS/rbs"
cp "$here/../assets/brand/rbs.icns" "$app/Contents/Resources/rbs.icns"
sed "s/@VERSION@/${version:-0.0.0}/g" "$here/Info.plist" > "$app/Contents/Info.plist"
identity=${RBS_SIGN_IDENTITY:-RBS Development}
if security find-certificate -c "$identity" >/dev/null 2>&1; then
    codesign --force --sign "$identity" "$app"
else
    codesign --force --sign - "$app"
    echo "Signed ad hoc: run $here/dev-identity.sh once to keep privacy grants across rebuilds." >&2
fi
echo "$app"
