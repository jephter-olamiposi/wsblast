#!/usr/bin/env bash
# Compares wsblast, k6 and Artillery against the same WebSocket echo server
# on one Linux machine. See docs/benchmarks.md for what each test measures
# and the results it recorded.
#
#   scripts/compare/run.sh
#
# Needs Linux (peak memory is read from /proc), taskset, python3, and the
# three tools. Point at them with WSBLAST, K6 and ARTILLERY if they are not
# on PATH. The echo server is `wsblast mock`, pinned to SERVER_CPU; each
# tool is pinned to TOOL_CPU, so "per core" means what it says.
set -euo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
WSBLAST=${WSBLAST:-wsblast}
K6=${K6:-k6}
ARTILLERY=${ARTILLERY:-artillery}
SERVER_CPU=${SERVER_CPU:-1}
TOOL_CPU=${TOOL_CPU:-0}
PORT=${PORT:-19600}
ECHO_CONNECTIONS=${ECHO_CONNECTIONS:-50}
ECHO_SECONDS=${ECHO_SECONDS:-30}
MEMORY_SMALL=${MEMORY_SMALL:-500}
MEMORY_LARGE=${MEMORY_LARGE:-2000}
MEMORY_SECONDS=${MEMORY_SECONDS:-20}
STALL_CONNECTIONS=${STALL_CONNECTIONS:-10}
STALL_RATE=${STALL_RATE:-20}
STALL_SECONDS=${STALL_SECONDS:-30}
OUT=${OUT:-$HERE/results}

export K6_NO_USAGE_REPORT=true ARTILLERY_DISABLE_TELEMETRY=true
URL="ws://127.0.0.1:$PORT"
mkdir -p "$OUT"
ulimit -n 16384 2>/dev/null || true
SERVER_PID=""
trap '[ -n "$SERVER_PID" ] && kill "$SERVER_PID" 2>/dev/null || true' EXIT

start_server() { # extra mock flags
  [ -n "$SERVER_PID" ] && kill "$SERVER_PID" 2>/dev/null && wait "$SERVER_PID" 2>/dev/null || true
  taskset -c "$SERVER_CPU" "$WSBLAST" mock --listen "127.0.0.1:$PORT" "$@" >/dev/null 2>&1 &
  SERVER_PID=$!
  for _ in $(seq 50); do
    (exec 3<>"/dev/tcp/127.0.0.1/$PORT") 2>/dev/null && return
    sleep 0.1
  done
  echo "the mock server did not start" >&2
  exit 1
}

# Runs a command pinned to TOOL_CPU and prints the peak resident memory of
# its whole process tree in KB (Artillery runs as several processes).
peak_tree_rss_kb() {
  taskset -c "$TOOL_CPU" "$@" >/dev/null 2>&1 &
  local root=$! peak=0 rss
  while kill -0 "$root" 2>/dev/null; do
    rss=$(ps -e -o pid=,ppid=,rss= | awk -v root="$root" '
      { ppid[$1] = $2; rss[$1] = $3 }
      END {
        for (p in rss) {
          q = p
          while (q != "" && q != 0 && q != 1) {
            if (q == root) { total += rss[p]; break }
            q = ppid[q]
          }
        }
        print total + 0
      }')
    [ "$rss" -gt "$peak" ] && peak=$rss
    sleep 0.1
  done
  wait "$root" || true
  echo "$peak"
}

json() { # file, python expression over d
  python3 -c "import json,sys; d=json.load(open(sys.argv[1])); print($2)" "$1"
}

versions() {
  echo "- wsblast: $("$WSBLAST" --version)"
  echo "- k6: $("$K6" version | head -n 1)"
  echo "- Artillery: $("$ARTILLERY" version 2>/dev/null | sed -n 's/^Artillery: *//p' | head -n 1)"
  echo "- CPU: $(sed -n 's/^model name\s*: //p' /proc/cpuinfo | head -n 1), $(nproc) cores"
  echo "- Kernel: $(uname -r)"
}

echo "## Environment"
echo
versions
echo

# Runs a command pinned to TOOL_CPU, with its output discarded, and prints
# the CPU time (user plus system, in seconds) it and its children used.
cpu_seconds() {
  python3 -c '
import resource, subprocess, sys
subprocess.run(sys.argv[1:], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
r = resource.getrusage(resource.RUSAGE_CHILDREN)
print(round(r.ru_utime + r.ru_stime, 2))' taskset -c "$TOOL_CPU" "$@"
}

# 1. Unpaced echo on one core: replies per second of wall time, and replies
# per second of the tool's own CPU time. When the tool does not use its
# whole core, the server is the limit and the second number is the one
# that compares the tools.
start_server
"$WSBLAST" "$URL" -c "$ECHO_CONNECTIONS" -d 2s --no-progress >/dev/null 2>&1 || true # warm up
ws_cpu=$(cpu_seconds "$WSBLAST" "$URL" -c "$ECHO_CONNECTIONS" -d "${ECHO_SECONDS}s" \
  --no-progress --format json -o "$OUT/echo-wsblast.json")
ws_n=$(json "$OUT/echo-wsblast.json" "d['metrics']['total_messages_recv']")
k6_cpu=$(cpu_seconds "$K6" run -q --summary-export "$OUT/echo-k6.json" \
  -e URL="$URL" -e VUS="$ECHO_CONNECTIONS" -e DURATION="${ECHO_SECONDS}s" "$HERE/k6-echo.js")
k6_n=$(json "$OUT/echo-k6.json" "d['metrics']['ws_msgs_received']['count']")
art_cpu=$(cd "$HERE" && VUS=$ECHO_CONNECTIONS DURATION=$ECHO_SECONDS \
  cpu_seconds "$ARTILLERY" run -o "$OUT/echo-artillery.json" -t "$URL" artillery-echo.yml)
art_n=$(json "$OUT/echo-artillery.json" "d['aggregate']['counters']['websocket.messages_received']")

echo "## Echo throughput, $ECHO_CONNECTIONS connections, one core"
echo
echo "| Tool | Replies per second | Tool CPU time | Replies per CPU second |"
echo "| :--- | ---: | ---: | ---: |"
row() { # tool, replies, cpu seconds
  python3 -c 'import sys
tool, n, cpu, secs = sys.argv[1], int(sys.argv[2]), float(sys.argv[3]), int(sys.argv[4])
print(f"| {tool} | {n / secs:,.0f} | {cpu:.1f} s | {n / cpu:,.0f} |")' "$1" "$2" "$3" "$ECHO_SECONDS"
}
row wsblast "$ws_n" "$ws_cpu"
row k6 "$k6_n" "$k6_cpu"
row Artillery "$art_n" "$art_cpu"
echo

# 2. Memory per connection: the slope of peak memory between two sizes,
# each connection sending one message a second.
mem() { # tool, connections
  case "$1" in
    wsblast) peak_tree_rss_kb "$WSBLAST" "$URL" -c "$2" -r 1 -d "${MEMORY_SECONDS}s" \
      --connect-timeout 30s --no-progress ;;
    k6) peak_tree_rss_kb "$K6" run -q -e URL="$URL" -e VUS="$2" -e RATE=1 \
      -e DURATION="${MEMORY_SECONDS}s" "$HERE/k6-paced.js" ;;
    artillery) (cd "$HERE" && VUS=$2 DURATION=$MEMORY_SECONDS \
      peak_tree_rss_kb "$ARTILLERY" run -t "$URL" artillery-paced.yml) ;;
  esac
}
echo "## Memory, $MEMORY_SMALL and $MEMORY_LARGE connections at 1 message per second each"
echo
echo "| Tool | Peak at $MEMORY_SMALL | Peak at $MEMORY_LARGE | Per additional connection |"
echo "| :--- | ---: | ---: | ---: |"
for tool in wsblast k6 artillery; do
  small=$(mem "$tool" "$MEMORY_SMALL")
  large=$(mem "$tool" "$MEMORY_LARGE")
  per=$(( (large - small) / (MEMORY_LARGE - MEMORY_SMALL) ))
  printf '| %s | %d MB | %d MB | %d KB |\n' "$tool" $((small / 1024)) $((large / 1024)) "$per"
done
echo

# 3. A server that stalls for 1 s every 10 s, under paced load.
start_server --stall-every 10s --stall-for 1s
taskset -c "$TOOL_CPU" "$WSBLAST" "$URL" -c "$STALL_CONNECTIONS" -r "$STALL_RATE" \
  -d "${STALL_SECONDS}s" --no-progress --format json -o "$OUT/stall-wsblast.json" >/dev/null 2>&1 || true
for script in paced closed; do
  taskset -c "$TOOL_CPU" "$K6" run -q --summary-export "$OUT/stall-k6-$script.json" \
    -e URL="$URL" -e VUS="$STALL_CONNECTIONS" -e RATE="$STALL_RATE" -e DURATION="${STALL_SECONDS}s" \
    "$HERE/k6-$script.js" >/dev/null 2>&1 || true
done
us() { json "$1" "'{:.0f} ms'.format(d['metrics']['$2']['$3'] / 1000)"; }
ms() { json "$1" "'{:.0f} ms'.format(d['metrics']['ws_rtt']['$2'])"; }

echo "## Stall: 1 s every 10 s, $STALL_CONNECTIONS connections at $STALL_RATE messages per second each"
echo
echo "| Tool and metric | p50 | p99 | max |"
echo "| :--- | ---: | ---: | ---: |"
f=$OUT/stall-wsblast.json
echo "| wsblast, response time (from schedule) | $(us "$f" response_time p50_us) | $(us "$f" response_time p99_us) | $(us "$f" response_time max_us) |"
echo "| wsblast, round-trip time | $(us "$f" message_rtt p50_us) | $(us "$f" message_rtt p99_us) | $(us "$f" message_rtt max_us) |"
f=$OUT/stall-k6-paced.json
echo "| k6, timer sends with timestamps | $(ms "$f" 'p(50)') | $(ms "$f" 'p(99)') | $(ms "$f" max) |"
f=$OUT/stall-k6-closed.json
echo "| k6, send then wait for the reply | $(ms "$f" 'p(50)') | $(ms "$f" 'p(99)') | $(ms "$f" max) |"
echo "| Artillery | not reported | not reported | not reported |"
