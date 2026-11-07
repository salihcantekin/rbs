#!/usr/bin/env bash
# Installs the Linux CI packages, reusing the .deb files kept in a cache directory.
#
# Usage: apt-install.sh <cache-dir> [--build-only]
#
# --build-only installs just the headers and libraries the build links (for clippy and
# rustdoc), not the Vulkan driver and daemons the tests use.
#
# The Azure Ubuntu mirror sometimes serves packages at a crawl (one run spent 13 minutes
# downloading these). The workflow restores <cache-dir> from the Actions cache, so apt
# only downloads what changed since main last saved it; new downloads are copied back so
# main's next run saves them. The package list is part of the cache key: changing it
# here starts a new cache.
set -euo pipefail

cache="$1"
build_only="${2:-}"
# Software Vulkan (lavapipe) so rbs-gpu golden image tests run instead of skipping.
# rbs-app (Slint, Skia renderer) links fontconfig and loads xkbcommon on Linux.
# PipeWire and libdbus headers build rbs-capture-pipewire; the daemon and WirePlumber
# let its tests link a test source to a capture stream, and its portal test runs a
# private session bus with dbus-daemon.
packages=(libfontconfig1-dev libxkbcommon-dev libpipewire-0.3-dev libdbus-1-dev)
if [ "$build_only" != --build-only ]; then
  packages+=(mesa-vulkan-drivers pipewire wireplumber dbus-daemon)
fi

mkdir -p "$cache"
if compgen -G "$cache/*.deb" > /dev/null; then
  sudo cp "$cache"/*.deb /var/cache/apt/archives/
fi
# Downloads that stall are cut off and fetched again (a network retry, not a test retry).
apt_opts=(-o Acquire::Retries=3 -o Acquire::http::Timeout=30 -o APT::Keep-Downloaded-Packages=true)
sudo apt-get "${apt_opts[@]}" update
sudo apt-get "${apt_opts[@]}" install -y "${packages[@]}"
if compgen -G "/var/cache/apt/archives/*.deb" > /dev/null; then
  cp /var/cache/apt/archives/*.deb "$cache"/
fi
