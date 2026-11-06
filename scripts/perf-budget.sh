#!/usr/bin/env bash
# Performance budget: fails when a release build of wsblast gets bigger,
# slower to start, heavier per connection or slower on echo than the limits
# below. CI runs it on Linux (the memory check reads /proc); run it locally
# with:
#
#   cargo build --release
#   scripts/perf-budget.sh target/release/wsblast
#
# The limits leave room for the noise of shared CI machines. They are meant
# to catch regressions such as a lock on the measurement path or memory that
# grows with every connection again, not drifts of a few percent. When a
# change moves a number on purpose, update the limit here in the same commit
# and say why in its message.
set -euo pipefail

# Limits, with what a build measured when they were set (Linux, 2 cores).
MAX_BINARY_MIB=12          # about 7 MiB
MAX_STARTUP_MS=100         # a few ms for --version
MAX_KB_PER_CONNECTION=48   # about 16 KB
MIN_ECHO_MSG_PER_SEC=5000  # about 25,000 with 50 connections

BIN=${1:?usage: scripts/perf-budget.sh path/to/wsblast}
BIN=$(cd "$(dirname "$BIN")" && pwd)/$(basename "$BIN")
WORK=$(mktemp -d)
PORT=${PERF_BUDGET_PORT:-19501}
MOCK_PID=""
cleanup() {
  [ -n "$MOCK_PID" ] && kill "$MOCK_PID" 2>/dev/null || true
  rm -rf "$WORK"
}
trap cleanup EXIT

# The memory check opens 4,000 connections, a socket each in wsblast and in
# the mock server.
ulimit -n 16384 2>/dev/null || true

failures=0
rows=()
check() { # name, value, limit, unit, ok(0/1)
  local verdict=pass
  if [ "$5" -ne 0 ]; then verdict=FAIL; failures=$((failures + 1)); fi
  rows+=("| $1 | $2 $4 | $3 $4 | $verdict |")
  printf '%-28s %10s %-6s limit %s %s: %s\n' "$1" "$2" "$4" "$3" "$4" "$verdict"
}

# Peak resident memory of a command, in KB, read from /proc while it runs.
peak_rss_kb() {
  "$@" >/dev/null 2>&1 &
  local pid=$! peak=0 rss
  while kill -0 "$pid" 2>/dev/null; do
    rss=$(awk '/^VmHWM:/ {print $2}' "/proc/$pid/status" 2>/dev/null || true)
    if [ -n "$rss" ] && [ "$rss" -gt "$peak" ]; then peak=$rss; fi
    sleep 0.05
  done
  wait "$pid" || true
  echo "$peak"
}

# 1. Binary size.
size=$(stat -c %s "$BIN")
size_mib=$(awk -v b="$size" 'BEGIN { printf "%.1f", b / 1048576 }')
check "Binary size" "$size_mib" "$MAX_BINARY_MIB" MiB \
  "$(awk -v s="$size_mib" -v m="$MAX_BINARY_MIB" 'BEGIN { print (s <= m) ? 0 : 1 }')"

# 2. Startup: median of 21 runs of --version.
times=()
for _ in $(seq 21); do
  start=$(date +%s%N)
  "$BIN" --version >/dev/null
  times+=($(( ($(date +%s%N) - start) / 1000000 )))
done
startup=$(printf '%s\n' "${times[@]}" | sort -n | sed -n 11p)
check "Startup (--version, median)" "$startup" "$MAX_STARTUP_MS" ms \
  "$([ "$startup" -le "$MAX_STARTUP_MS" ] && echo 0 || echo 1)"

# The mock server is the target of the remaining checks.
"$BIN" mock --listen "127.0.0.1:$PORT" >/dev/null 2>&1 &
MOCK_PID=$!
for _ in $(seq 50); do
  (exec 3<>"/dev/tcp/127.0.0.1/$PORT") 2>/dev/null && break
  sleep 0.1
done
URL="ws://127.0.0.1:$PORT"

# 3. Memory per connection: the slope between 2,000 and 4,000 paced
# connections. The runtime's fixed cost cancels out, and so do the read
# buffers, which share a fixed 64 MiB from 512 connections on.
small=$(peak_rss_kb "$BIN" "$URL" -c 2000 -d 4s -r 1 --no-progress)
large=$(peak_rss_kb "$BIN" "$URL" -c 4000 -d 4s -r 1 --no-progress)
per_conn=$(( (large - small) / 2000 ))
check "Memory per connection" "$per_conn" "$MAX_KB_PER_CONNECTION" KB \
  "$([ "$per_conn" -le "$MAX_KB_PER_CONNECTION" ] && echo 0 || echo 1)"

# 4. Unpaced echo throughput with 50 connections.
"$BIN" "$URL" -c 50 -d 5s --no-progress --format json -o "$WORK/echo.json" >/dev/null 2>&1
throughput=$(sed -n 's/.*"throughput_msg_per_sec": *\([0-9]*\).*/\1/p' "$WORK/echo.json" | head -n 1)
check "Echo throughput, 50 conns" "${throughput:-0}" "$MIN_ECHO_MSG_PER_SEC" msg/s \
  "$([ "${throughput:-0}" -ge "$MIN_ECHO_MSG_PER_SEC" ] && echo 0 || echo 1)"

if [ -n "${GITHUB_STEP_SUMMARY:-}" ]; then
  {
    echo "### Performance budget"
    echo
    echo "| Check | Measured | Limit | Result |"
    echo "| :--- | ---: | ---: | :--- |"
    printf '%s\n' "${rows[@]}"
    echo
    echo "Peak resident memory: $small KB with 2,000 connections, $large KB with 4,000."
  } >> "$GITHUB_STEP_SUMMARY"
fi

if [ "$failures" -gt 0 ]; then
  echo "$failures check(s) over budget" >&2
  exit 1
fi
echo "All checks within budget."
