#!/usr/bin/env bash
# Clean and incremental release build times, binary size and dependency counts.
# Each prototype builds in its own target directory so no artefacts are shared.
set -eu
OUT=${1:-/tmp/rbs-proto-build}
mkdir -p "$OUT"
echo "| prototype | clean release build s | incremental after UI edit s | binary MiB (stripped) | crates in tree | shared libs outside libc/Mesa |"
echo "| --- | --- | --- | --- | --- | --- |"
for p in slint-proto iced-proto qt-proto; do
  T="$OUT/$p"; rm -rf "$T"
  cargo fetch -q
  s=$(date +%s.%N); CARGO_TARGET_DIR=$T cargo build -q --release -p $p 2>/dev/null; e=$(date +%s.%N)
  clean=$(echo "$s $e" | awk '{printf "%.0f", $2-$1}')
  case $p in
    slint-proto) f=slint-proto/ui/app.slint ;;
    iced-proto) f=iced-proto/src/main.rs ;;
    qt-proto) f=qt-proto/qml/main.qml ;;
  esac
  echo "// touch" >> $f
  s=$(date +%s.%N); CARGO_TARGET_DIR=$T cargo build -q --release -p $p 2>/dev/null; e=$(date +%s.%N)
  sed -i '$ d' $f
  inc=$(echo "$s $e" | awk '{printf "%.1f", $2-$1}')
  size=$(stat -c %s "$T/release/$p" | awk '{printf "%.1f", $1/1048576}')
  crates=$(cargo tree -q -p $p -e normal,build --prefix none 2>/dev/null | sed 's/ (\*)//' | sort -u | wc -l)
  libs=$(ldd "$T/release/$p" | awk '{print $1}' | grep -vE 'linux-vdso|libc\.so|libm\.so|libgcc|ld-linux|libstdc|libpthread|libdl|librt' | tr '\n' ' ')
  printf "| %s | %s | %s | %s | %s | %s |\n" $p "$clean" "$inc" "$size" "$crates" "$libs"
done
