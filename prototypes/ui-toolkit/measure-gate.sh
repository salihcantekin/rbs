#!/usr/bin/env bash
# ADR-0022 acceptance-gate measurements on a machine with a real GPU (macOS or Linux desktop).
# Builds the prototypes that can be built here, runs each scenario with the
# built-in self-measurement (presented frames, process CPU, peak RSS) and
# prints one Markdown table. Qt is skipped when qmake6/qmake is not installed.
# Usage: ./measure-gate.sh [seconds per scenario, default 20]
set -u
SECS=${1:-20}
cd "$(dirname "$0")"
PROTOS="slint-proto iced-proto"
if command -v qmake6 >/dev/null || command -v qmake >/dev/null; then PROTOS="$PROTOS qt-proto"; fi
for p in $PROTOS; do cargo build -q --release -p "$p" || exit 1; done

idle_cpu() { # prototype -> CPU % over SECS with no animated panel visible
  RBS_PROTO_CLOSE=rbs.program,rbs.mixer "target/release/$1" >/dev/null 2>&1 &
  local pid=$!; sleep 4
  local a; a=$(ps -o time= -p $pid | awk -F'[:.]' '{print ($1*60+$2)+$3/100}')
  sleep "$SECS"
  local b; b=$(ps -o time= -p $pid | awk -F'[:.]' '{print ($1*60+$2)+$3/100}')
  local rss; rss=$(ps -o rss= -p $pid | awk '{printf "%.0f", $1/1024}')
  kill $pid; wait $pid 2>/dev/null
  echo "$a $b $SECS $rss" | awk '{printf "cpu_pct=%.1f rss_mib=%s", 100*($2-$1)/$3, $4}'
}

scenario() { # prototype name env...
  local p=$1 name=$2; shift 2
  local out; out=$(env "$@" RBS_PROTO_MEASURE="$SECS" "target/release/$p" 2>/dev/null | grep '^MEASURE' | cut -d' ' -f3-)
  echo "| $p | $name | ${out:-no frames (iced has no presented-frame hook; not measured)} |"
}

echo "Machine: $(uname -srm); $(sysctl -n machdep.cpu.brand_string 2>/dev/null || grep -m1 'model name' /proc/cpuinfo | cut -d: -f2)"
echo
echo "| prototype | scenario | result |"
echo "| --- | --- | --- |"
for p in $PROTOS; do
  echo "| $p | idle, no animated panel | $(idle_cpu $p) |"
  [ "$p" = iced-proto ] && continue
  scenario $p "1080p preview + 64-band spectrum" RBS_PROTO_SIZE=1920x1080
  scenario $p "1080p preview only" RBS_PROTO_SIZE=1920x1080 RBS_PROTO_CLOSE=rbs.mixer
  [ "$p" = slint-proto ] && scenario $p "1080p preview only, engine idle (static frame)" RBS_PROTO_SIZE=1920x1080 RBS_PROTO_CLOSE=rbs.mixer RBS_PROTO_STATIC=1
  scenario $p "dense path, 1024 points" RBS_PROTO_BANDS=1024 RBS_PROTO_CLOSE=rbs.program
  scenario $p "dense path, 4096 points" RBS_PROTO_BANDS=4096 RBS_PROTO_CLOSE=rbs.program
done
echo
echo "Late = frame interval over 25 ms (1.5 periods at 60 Hz). Gate (ADR-0022): late_pct <= 1 and cpu_pct <= 10."
