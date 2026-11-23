# macOS bundle helpers for tools/dev/build.sh (sourced, not run).
#
# runner_bundle <build dir> <dest dir>
#   Makes <dest dir>/rbs-browser-cef.app from rbs-browser-cef and
#   rbs-browser-cef-helper in <build dir>. CEF on macOS starts its renderer, GPU and
#   utility processes from helper apps in Contents/Frameworks, named
#   "rbs-browser-cef Helper (GPU)" and so on (the layout of the cef crate's
#   bundle-cef-app, without the framework: the runner loads the framework from the
#   installed engine in RBS_CEF_RUNTIME, `rbs-browser-cef` `engine.rs`).
#
# sign_app <RBS.app>
#   Signs the nested apps first, then RBS.app, with "RBS Development" (or
#   $RBS_SIGN_IDENTITY) when the keychain has it, else ad hoc, like
#   crates/rbs-app/macos/bundle.sh.

runner_plist() { # <plist path> <executable> <identifier> <helper: 0|1>
    ui_element=
    if [ "$4" = 1 ]; then ui_element='    <key>LSUIElement</key><string>1</string>'; fi
    cat > "$1" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key><string>$2</string>
    <key>CFBundleDisplayName</key><string>$2</string>
    <key>CFBundleIdentifier</key><string>$3</string>
    <key>CFBundleExecutable</key><string>$2</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>CFBundleInfoDictionaryVersion</key><string>6.0</string>
    <key>CFBundleVersion</key><string>1.0.0</string>
    <key>CFBundleShortVersionString</key><string>1.0.0</string>
    <key>LSMinimumSystemVersion</key><string>13.0</string>
    <key>LSEnvironment</key><dict><key>MallocNanoZone</key><string>0</string></dict>
    <key>NSSupportsAutomaticGraphicsSwitching</key><true/>
$ui_element
</dict>
</plist>
EOF
}

runner_bundle() { # <build dir> <dest dir>
    name=rbs-browser-cef
    rb_app="$2/$name.app"
    rm -rf "$rb_app"
    mkdir -p "$rb_app/Contents/MacOS" "$rb_app/Contents/Frameworks" "$rb_app/Contents/Resources"
    cp "$1/$name" "$rb_app/Contents/MacOS/$name"
    runner_plist "$rb_app/Contents/Info.plist" "$name" app.rbs.browser-cef 0
    for kind in "Helper (GPU)" "Helper (Renderer)" "Helper (Plugin)" "Helper (Alerts)" "Helper"; do
        helper="$name $kind"
        id=$(printf '%s' "$kind" | tr -d '()' | tr ' ' '.' | tr '[:upper:]' '[:lower:]')
        dir="$rb_app/Contents/Frameworks/$helper.app"
        mkdir -p "$dir/Contents/MacOS"
        cp "$1/$name-helper" "$dir/Contents/MacOS/$helper"
        runner_plist "$dir/Contents/Info.plist" "$helper" "app.rbs.browser-cef.$id" 1
    done
}

sign_app() { # <RBS.app>
    identity=${RBS_SIGN_IDENTITY:-RBS Development}
    if ! security find-certificate -c "$identity" >/dev/null 2>&1; then identity=-; fi
    runner="$1/Contents/MacOS/rbs-browser-cef.app"
    if [ -d "$runner" ]; then
        for helper in "$runner"/Contents/Frameworks/*.app; do
            codesign --force --sign "$identity" "$helper"
        done
        codesign --force --sign "$identity" "$runner"
    fi
    if [ -f "$1/Contents/MacOS/rbs-mcp" ]; then
        codesign --force --sign "$identity" "$1/Contents/MacOS/rbs-mcp"
    fi
    codesign --force --sign "$identity" "$1"
}
