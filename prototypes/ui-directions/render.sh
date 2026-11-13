#!/usr/bin/env bash
# Renders every direction, theme and screen of the design study to out/*.png with
# Slint's own renderer (slint-viewer 1.18.1 --screenshot, no display needed).
# Usage: ./render.sh [scale]   (cargo install slint-viewer --version 1.18.1 --locked)
set -euo pipefail
cd "$(dirname "$0")"
scale="${1:-1}"
mkdir -p out .variants
names=(main settings first-run sheet)
letters=(a b c)
for d in 0 1 2; do for t in dark light; do for s in 0 1 2 3; do
  f=".variants/v.slint"
  printf 'import { App } from "../ui/main.slint";\nexport component V inherits App { direction: %s; dark: %s; screen: %s; }\n' \
    "$d" "$([ "$t" = dark ] && echo true || echo false)" "$s" > "$f"
  SLINT_SCALE_FACTOR="$scale" "${VIEWER:-slint-viewer}" --screenshot "out/${letters[$d]}-${names[$s]}-$t.png" "$f"
done; done; done
ls out | wc -l
