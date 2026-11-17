#!/bin/bash
# RBS cloud environment setup (docs/specs/testing-strategy.md, "Cloud containers").
# The environment's setup script is a copy of this file. Runs as root before Claude
# starts, from the folder that holds the checkouts; when it finishes in
# under about 5 minutes the file system is cached for later sessions (about 7 days).
set -euo pipefail
export DEBIAN_FRONTEND=noninteractive PATH="$HOME/.cargo/bin:$PATH"
apt-get update -qq
apt-get install -y -qq mesa-vulkan-drivers libpipewire-0.3-dev libspa-0.2-dev clang \
  libclang-dev libdbus-1-dev libxkbcommon-dev libxkbcommon-x11-0 libfontconfig1-dev \
  libwayland-dev libudev-dev libasound2-dev >/dev/null
curl -LsSf https://get.nexte.st/latest/linux | tar zxf - -C "$HOME/.cargo/bin"
# Prebuild third-party crates only (cargo-chef stubs the workspace), into the repo's
# target/, so a session compiles just the workspace crates. A failure here must not stop
# the session from starting: it then compiles the dependencies itself.
if [ -f rbs/Cargo.toml ]; then
  root="$PWD/rbs"; chef=$(mktemp -d)
  cargo install cargo-chef --locked -q \
    && (cd "$root" && cargo chef prepare --recipe-path "$chef/recipe.json") \
    && cp -r "$root/.cargo" "$root/rust-toolchain.toml" "$chef/" \
    && (cd "$chef" && CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=4 cargo chef cook \
      --recipe-path recipe.json --workspace --tests --examples --target-dir "$root/target") \
    || echo "rbs setup: dependency prebuild failed; sessions compile them instead"
  rm -rf "$chef"
fi
