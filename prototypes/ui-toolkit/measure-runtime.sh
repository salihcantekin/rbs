#!/usr/bin/env bash
# Runtime measurements for the three prototypes on a virtual X display.
# Usage: ./measure-runtime.sh [seconds]   (needs Xvfb, a release build, and Mesa)
set -u
SECS=${1:-10}
BIN=target/release
export DISPLAY=${DISPLAY:-:99}
pgrep -x Xvfb >/dev/null || { Xvfb "$DISPLAY" -screen 0 1600x1000x24 >/dev/null 2>&1 & sleep 1; }
export SLINT_WGPU_CPU=1 RBS_PROTO_FPS=1 RBS_PROTO_DIR=$(mktemp -d)
TCK=$(getconf CLK_TCK)

cpu_ticks() { awk '{print $14+$15}' "/proc/$1/stat"; }

run() { # name scenario close-list
  local name=$1 scen=$2 close=$3 log; log=$(mktemp)
  RBS_PROTO_CLOSE=$close "$BIN/$name" >"$log" 2>&1 &
  local pid=$!
  sleep 4
  local c0; c0=$(cpu_ticks $pid); local t0; t0=$(date +%s.%N)
  sleep "$SECS"
  local c1; c1=$(cpu_ticks $pid); local t1; t1=$(date +%s.%N)
  local rss; rss=$(awk '/VmRSS/{print $2}' /proc/$pid/status)
  local thr; thr=$(awk '/Threads/{print $2}' /proc/$pid/status)
  kill $pid; wait $pid 2>/dev/null
  local fps; fps=$(grep '^fps' "$log" | tail -3 | awk '{s+=$2;n++} END{if(n) printf "%.0f", s/n; else print "-"}')
  local cpu; cpu=$(echo "$c0 $c1 $t0 $t1 $TCK" | awk '{printf "%.1f", 100*($2-$1)/$5/($4-$3)}')
  local first; first=$(grep -o 'first [a-z]* after [0-9]* ms' "$log" | grep -o '[0-9]*' | head -1)
  printf "| %s | %s | %s | %s | %s | %s |\n" "$name" "$scen" "$cpu" "$((rss/1024))" "$thr" "$fps"
  rm -f "$log"
}

echo "| prototype | scenario | CPU % of one core | RSS MiB | threads | ticks/s |"
echo "| --- | --- | --- | --- | --- | --- |"
for p in slint-proto iced-proto qt-proto; do
  run $p "idle (no animated panel)" "rbs.program,rbs.mixer"
  run $p "spectrum only, 64 bands" "rbs.program"
  run $p "program preview only" "rbs.mixer"
done
