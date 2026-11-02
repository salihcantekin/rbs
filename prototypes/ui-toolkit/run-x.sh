#!/bin/sh
# Start a virtual X display with a software Vulkan driver (lavapipe) for headless runs.
export DISPLAY=:99
pgrep -x Xvfb >/dev/null || (Xvfb :99 -screen 0 1600x1000x24 >/dev/null 2>&1 &)
sleep 1
