#!/usr/bin/env bash
# Start each prototype with an AT-SPI bus and print what a screen reader sees.
export DISPLAY=${DISPLAY:-:99} SLINT_WGPU_CPU=1 QT_LINUX_ACCESSIBILITY_ALWAYS_ON=1 NO_AT_BRIDGE=0
eval "$(dbus-launch --sh-syntax)"
/usr/libexec/at-spi-bus-launcher --launch-immediately >/dev/null 2>&1 &
sleep 1
gsettings set org.gnome.desktop.interface toolkit-accessibility true 2>/dev/null
for p in slint-proto iced-proto qt-proto; do
  echo "=== $p"
  RBS_PROTO_CLOSE=rbs.program,rbs.mixer timeout 12 target/release/$p >/dev/null 2>&1 &
  sleep 6
  /usr/bin/python3.12 -I a11y-dump.py | tail -${A11Y_LINES:-12}
  sleep 6
done
kill "$DBUS_SESSION_BUS_PID" 2>/dev/null
