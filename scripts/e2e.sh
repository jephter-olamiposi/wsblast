#!/bin/bash
# End-to-end checks of every wsblast feature, run against the real binary as
# a user would: exit codes, report formats, scenarios, capture and replay,
# exports, comparison, chaos, authentication, TLS, distributed runs, data
# files, reply checks, named actions, arrival rates, source addresses,
# Socket.IO and compression. Every server it needs is started locally
# (wsblast mock, agents, small Python stand-ins for an OTLP collector and an
# OAuth2 token endpoint). When the Python `websockets` package and node with
# npm are available, compression and Socket.IO are also checked against
# independent implementations: the websockets server and client, and the
# socket.io 2 and 4 servers and clients, which npm installs into the
# temporary directory.
#
#   scripts/e2e.sh [path/to/wsblast]
#
# The binary must be built with every feature:
#   cargo build --release --all-features && scripts/e2e.sh target/release/wsblast
# Needs bash, python3, curl and openssl; the source-address and TUI checks
# run on Linux only. Uses ports 19001-19034, 19443-19444, 19464, 19501-19503,
# 14318, 14444 and 17701-17703 on 127.0.0.1. Exits 1 when any check fails.
W=${1:-${WSBLAST:-wsblast}}
case "$W" in */*) W=$(cd "$(dirname "$W")" && pwd)/$(basename "$W") ;; esac
"$W" --version >/dev/null 2>&1 || { echo "cannot run $W" >&2; exit 1; }
D=$(mktemp -d "${TMPDIR:-/tmp}/wsblast-e2e.XXXXXX")
cd "$D" || exit 1
LINUX=0; [ "$(uname -s)" = Linux ] && LINUX=1
PASS=0; FAIL=0
ok()  { echo "PASS  $1"; PASS=$((PASS+1)); }
bad() { echo "FAIL  $1  ($2)"; FAIL=$((FAIL+1)); }
check() { if eval "$2"; then ok "$1"; else bad "$1" "$3"; fi; }
# Checks against independent implementations need third-party packages.
# They are skipped when the packages are missing, unless
# WSBLAST_E2E_INTEROP=require, as in CI, makes a skip a failure.
skip() { if [ "${WSBLAST_E2E_INTEROP:-}" = require ]; then bad "$1" "skipped"; else echo "SKIP  $1"; fi; }
j() { python3 -c "import json,sys; d=json.load(open('$1')); print($2)"; }
cleanup() { kill $(jobs -p) 2>/dev/null; wait 2>/dev/null; cd /; rm -rf "$D"; }
trap cleanup EXIT

$W mock --listen 127.0.0.1:19001 >mock.log 2>&1 &
$W mock --listen 127.0.0.1:19002 --delay 5ms --stall-every 1s --stall-for 200ms >mock2.log 2>&1 &
sleep 1
T=ws://127.0.0.1:19001; S=ws://127.0.0.1:19002

# Core run, formats, exit codes
$W $T -c 10 -d 2s --no-progress >run.txt 2>&1; rc=$?
check "basic run exits 0 with a summary" "[ $rc = 0 ] && grep -q 'WebSocket Benchmark Summary' run.txt" "rc=$rc"
for f in json markdown html; do $W $T -c 5 -d 1s --no-progress --format $f -o out.$f >/dev/null 2>&1; done
check "json report parses" "j out.json 'd[\"metrics\"][\"total_messages_recv\"]>0' | grep -q True" "bad json"
check "markdown report" "grep -q '|' out.markdown" ""
check "html report is self-contained" "grep -q '<svg class=\"chart\"' out.html && ! grep -q '<script' out.html" ""
$W $S -c 10 -d 3s -r 20 --max-p99 20ms --no-progress >/dev/null 2>&1; rc=$?
check "SLO breach exits 2" "[ $rc = 2 ]" "rc=$rc"
$W $S -c 10 -d 2s -r 20 --max-p99 2s --no-progress >/dev/null 2>&1; rc=$?
check "SLO pass exits 0" "[ $rc = 0 ]" "rc=$rc"
$W ws://127.0.0.1:1 -c 2 -d 1s --no-progress >/dev/null 2>&1; rc=$?
check "unreachable target exits 1" "[ $rc = 1 ]" "rc=$rc"
$W notaurl >/dev/null 2>&1; rc=$?
check "invalid URL exits 1" "[ $rc = 1 ]" "rc=$rc"

# Coordinated omission correction and exact -n
$W $S -c 10 -d 3s -r 20 --no-progress --format json -o co.json >/dev/null 2>&1
check "response time p95 > RTT p95 under stalls" "j co.json 'd[\"metrics\"][\"response_time\"][\"p95_us\"] > d[\"metrics\"][\"message_rtt\"][\"p95_us\"]' | grep -q True" "$(j co.json "d['metrics']['response_time']['p95_us'], d['metrics']['message_rtt']['p95_us']" 2>&1)"
$W $T -c 40 -n 101 -d 10s --no-progress --format json -o n.json >/dev/null 2>&1
check "-n 101 sends exactly 101" "[ \"\$(j n.json 'd[\"metrics\"][\"total_messages_sent\"]')\" = 101 ]" "$(j n.json "d['metrics']['total_messages_sent']")"

# Stages and time series
$W $T --stages 1s:10,1s:10,1s:0 -r 10 --no-progress --format json -o st.json >/dev/null 2>&1
check "stages, rate and time series in JSON" "j st.json 'len(d[\"metrics\"][\"time_series\"][\"points\"])>=3 and len(d[\"stages\"])==3 and d[\"rate_per_conn\"]==10' | grep -q True" ""
$W render st.json -o st.html >/dev/null 2>&1
check "render shows the saved stages" "grep -q 'Load stages' st.html" ""
$W $T -c 3 -d 1s --no-progress --format html -o quiet.html >quiet.out 2>&1
check "-o writes the file, terminal gets the summary" "grep -q '<!doctype html>' quiet.html && grep -q 'WebSocket Benchmark Summary' quiet.out && ! grep -q '<!doctype' quiet.out" ""

# Scenario with phase SLOs and stages
cat > sc.yaml <<EOF
version: "1.0"
metadata:
  name: e2e
target: "$T"
phases:
  - name: warm
    duration: "1s"
    connections: 5
    rate_per_conn: 10
    slo: { max_error_rate: 0.01 }
  - name: ramp
    stages: "1s:10,1s:0"
    rate_per_conn: 10
EOF
$W --scenario sc.yaml --no-progress --format json -o sc.json >/dev/null 2>&1; rc=$?
check "scenario runs with per-phase results" "[ $rc = 0 ] && j sc.json 'len(d[\"phases\"])' | grep -q 2" "rc=$rc"

# Capture, then mock replay of the capture
$W $T -c 3 -d 1s --no-progress --capture cap.jsonl >cap.txt 2>&1
check "capture writes JSON Lines" "[ \$(wc -l < cap.jsonl) -gt 3 ] && head -1 cap.jsonl | python3 -c 'import json,sys; json.loads(sys.stdin.readline())'" ""
$W mock --listen 127.0.0.1:19003 --replay cap.jsonl >mock3.log 2>&1 &
sleep 1
$W ws://127.0.0.1:19003 --mode listen -c 1 -d 1s --no-progress --format json -o replay.json >/dev/null 2>&1
check "mock replays a capture" "j replay.json 'd[\"metrics\"][\"total_messages_recv\"]>0' | grep -q True" ""

# Prometheus live endpoint
$W $T -c 5 -d 3s --no-progress --prometheus-listen 127.0.0.1:19464 >prom.txt 2>&1 &
sleep 2; curl -s http://127.0.0.1:19464/metrics > prom.out; wait $!
check "prometheus /metrics during run" "grep -q '^wsblast_messages_sent' prom.out" "$(head -c 200 prom.out)"

# OpenTelemetry to a fake collector
python3 - >otelc.log 2>&1 <<'EOF' &
import http.server
class H(http.server.BaseHTTPRequestHandler):
    def do_POST(self):
        n=int(self.headers.get('content-length',0)); body=self.rfile.read(n)
        open('otel.bin','ab').write(body); self.send_response(200); self.send_header('content-length','0'); self.end_headers()
    def log_message(self,*a): pass
http.server.HTTPServer(('127.0.0.1',14318),H).serve_forever()
EOF
sleep 1
$W $T -c 3 -d 2s --no-progress --otel-endpoint http://127.0.0.1:14318 --otel-interval 500ms >otel.txt 2>&1
check "otel export reaches collector" "grep -q 'wsblast.messages.sent' otel.bin && ! grep -q Warning otel.txt" "$(cat otel.txt | tail -2)"

# Compare, window, regression gate, compare HTML, render
$W $T -c 5 -d 3s -r 20 --no-progress --format json -o base.json >/dev/null 2>&1
$W $S -c 5 -d 3s -r 20 --no-progress --format json -o cur.json >/dev/null 2>&1
$W --compare base.json cur.json --fail-on-regression >cmp.txt 2>&1; rc=$?
check "compare flags a regression with exit 2" "[ $rc = 2 ]" "rc=$rc"
$W --compare base.json cur.json --compare-window 1s.. --format json -o cmp.json >/dev/null 2>&1
check "compare window writes schema 1.2" "j cmp.json 'd[\"schema_version\"]' | grep -q 1.2" ""
$W --compare base.json cur.json --format html -o cmp.html >/dev/null 2>&1
check "compare HTML" "grep -q 'baseline comparison' cmp.html" ""
$W render cur.json -o r.html >/dev/null 2>&1; rc=$?
$W render cmp.json --format text >rcmp.txt 2>&1; rc2=$?
check "render report and comparison" "[ $rc = 0 ] && [ $rc2 = 0 ] && grep -q '<svg' r.html" "rc=$rc rc2=$rc2"

# Chaos and generator lag
$W $T -c 5 -d 2s --no-progress --chaos-drop 0.2 --chaos-seed 7 --format json -o chaos.json >chaos.txt 2>&1
check "chaos injection recorded" "grep -qi chaos chaos.json" ""
$W $T -c 5 -d 2s --no-progress --max-generator-lag 1ms >lag.txt 2>&1; rc=$?
check "generator-lag gate runs (exit 0 or 2)" "[ $rc = 0 ] || [ $rc = 2 ]" "rc=$rc"

# Auth: static token, token command, JWT, OAuth2
$W $T -c 2 -d 1s --no-progress --auth-type static --auth-token abc >/dev/null 2>&1; rc=$?
check "static bearer token" "[ $rc = 0 ]" "rc=$rc"
$W $T -c 2 -d 1s --no-progress --auth-type static --auth-token-cmd 'echo tok123' >/dev/null 2>&1; rc=$?
check "token from command" "[ $rc = 0 ]" "rc=$rc"
printf 'supersecretkeysupersecretkey1234' > jwt.key
$W $T -c 2 -d 1s --no-progress --auth-type jwt --auth-jwt-key jwt.key --auth-jwt-claims '{"sub":"t"}' >jwt.txt 2>&1; rc=$?
check "JWT signing" "[ $rc = 0 ]" "rc=$rc $(tail -1 jwt.txt)"
python3 - >oauth.log 2>&1 <<'EOF' &
import http.server, json
class H(http.server.BaseHTTPRequestHandler):
    def do_POST(self):
        self.rfile.read(int(self.headers.get('content-length',0)))
        b=json.dumps({"access_token":"tok","token_type":"Bearer","expires_in":3600}).encode()
        self.send_response(200); self.send_header('content-type','application/json'); self.send_header('content-length',str(len(b))); self.end_headers(); self.wfile.write(b)
    def log_message(self,*a): pass
http.server.HTTPServer(('127.0.0.1',14444),H).serve_forever()
EOF
sleep 1; printf 's3cret' > oauth.secret
$W $T -c 2 -d 1s --no-progress --auth-type oauth2 --auth-oauth2-token-url http://127.0.0.1:14444/token --auth-oauth2-client-id cid --auth-oauth2-client-secret-file oauth.secret >oauth.txt 2>&1; rc=$?
check "OAuth2 client credentials" "[ $rc = 0 ]" "rc=$rc $(tail -2 oauth.txt)"

# TLS: mock over wss with a self-signed cert
openssl req -x509 -newkey rsa:2048 -nodes -keyout k.pem -out c.pem -days 1 -subj /CN=localhost -addext subjectAltName=DNS:localhost,IP:127.0.0.1 -addext basicConstraints=critical,CA:FALSE >/dev/null 2>&1
$W mock --listen 127.0.0.1:19443 --tls-cert c.pem --tls-key k.pem >mtls.log 2>&1 &
sleep 1
SSL_CERT_FILE=c.pem $W wss://127.0.0.1:19443 -c 3 -d 1s --no-progress >tls.txt 2>&1; rc=$?
check "wss to TLS mock" "[ $rc = 0 ]" "rc=$rc $(tail -2 tls.txt)"

# Distributed: two agents, token, exact merge, scenario across agents
TOK=0123456789abcdef0123
$W agent --listen 127.0.0.1:17701 --token $TOK >a1.log 2>&1 &
$W agent --listen 127.0.0.1:17702 --token $TOK >a2.log 2>&1 &
sleep 1
$W $T -c 10 -d 2s -n 300 --no-progress --agents 127.0.0.1:17701,127.0.0.1:17702 --agent-token $TOK --format json -o dist.json >dist.txt 2>&1; rc=$?
check "distributed run over 2 agents" "[ $rc = 0 ] && j dist.json 'd[\"metrics\"][\"distributed\"][\"agents\"]' | grep -q 2" "rc=$rc $(tail -2 dist.txt)"
check "distributed -n is exact across agents" "[ \"\$(j dist.json 'd[\"metrics\"][\"total_messages_sent\"]')\" = 300 ]" "$(j dist.json "d['metrics']['total_messages_sent']")"
$W --scenario sc.yaml --no-progress --agents 127.0.0.1:17701,127.0.0.1:17702 --agent-token $TOK >dsc.txt 2>&1; rc=$?
check "scenario across agents" "[ $rc = 0 ]" "rc=$rc $(tail -2 dsc.txt)"
$W $T -c 2 -d 1s --no-progress --agents 127.0.0.1:17701 --agent-token wrongwrongwrongwrong >dbad.txt 2>&1; rc=$?
check "wrong agent token rejected" "[ $rc = 1 ]" "rc=$rc"
$W agent --listen 127.0.0.1:17703 --token short >ashort.txt 2>&1; rc=$?
check "short agent token refused" "[ $rc = 1 ]" "rc=$rc"

# 0.2.3: data files, reply checks, captures, named actions, arrivals,
# source addresses, Socket.IO
$W mock --listen 127.0.0.1:19033 --socketio --delay 2ms >mock4.log 2>&1 &
sleep 0.5
printf 'user,token\nada,t1\nbob,t2\ncy,t3\n' > users.csv
$W $T -c 3 -n 9 -d 5s --data users.csv -p '{"user":"{{data.user}}","seq":{{seq}}}' --expect 'json:/user' --no-progress --format json -o data.json >/dev/null 2>&1; rc=$?
check "--data rows reach the server and --expect passes" "[ $rc = 0 ] && [ \"\$(j data.json 'd[\"metrics\"][\"error_rate\"]')\" = 0.0 ]" "rc=$rc"
$W $T -c 2 -d 1s -p '{"user":"ada"}' --expect 'json:/user=bob' --max-error-rate 0.01 --no-progress --format json -o expectfail.json >/dev/null 2>&1; rc=$?
check "failed --expect counts check_failed and gates" "[ $rc = 2 ] && j expectfail.json '\"check_failed\" in d[\"metrics\"][\"error_breakdown\"]' | grep -q True" "rc=$rc"
cat > actions.yaml <<'YAML'
version: "1.0"
metadata: { name: e2e-actions }
phases:
  - name: session
    connections: 3
    duration: 2s
    actions:
      - type: Send
        name: login
        payload: '{"op":"login","session":"s-{{worker_id}}"}'
        capture: { session: /session }
        slo: { max_p99: 2s, max_error_rate: 0 }
      - type: Send
        name: subscribe
        count: 3
        payload: '{"op":"sub","session":"{{session}}","seq":{{seq}}}'
        expect: { json: /session }
      - type: Wait
        duration: 100ms
YAML
$W --scenario actions.yaml $T --no-progress --format json -o actions.json >/dev/null 2>&1; rc=$?
check "scenario captures, checks and named actions" "[ $rc = 0 ] && j actions.json 'sorted(a[\"name\"] for a in d[\"metrics\"][\"actions\"])' | grep -q \"\['login', 'subscribe'\]\"" "rc=$rc"
sed 's/max_p99: 2s/max_p99: 1us/' actions.yaml > actions-fail.yaml
$W --scenario actions-fail.yaml $T --no-progress >actions-fail.txt 2>&1; rc=$?
check "an action SLO breach exits 2" "[ $rc = 2 ] && grep -q 'Action login: latency p99' actions-fail.txt" "rc=$rc"
$W $T --arrival-rate 20 --session-duration 500ms -d 2s -r 5 --no-progress --format json -o arr.json >/dev/null 2>&1; rc=$?
check "--arrival-rate starts its sessions" "[ $rc = 0 ] && j arr.json '38 <= d[\"metrics\"][\"arrivals\"][\"started\"] <= 40 and d[\"metrics\"][\"arrivals\"][\"skipped\"] == 0 and d[\"metrics\"][\"total_connections_established\"] == d[\"metrics\"][\"arrivals\"][\"started\"]' | grep -q True" "rc=$rc $(j arr.json 'd[\"metrics\"].get(\"arrivals\")' 2>&1)"
$W $T --arrival-rate 50 --session-duration 2s --max-connections 10 -d 1s --no-progress >cap.txt 2>&1; rc=$?
check "--max-connections skips arrivals with a note" "[ $rc = 0 ] && grep -q 'raise --max-connections' cap.txt" "rc=$rc"
# Linux routes all of 127.0.0.0/8 to loopback, so 127.0.0.2 is local there.
if [ $LINUX = 1 ]; then
$W $T -c 4 -d 1s --source-ip 127.0.0.1,127.0.0.2 --no-progress --format json -o src.json >/dev/null 2>&1; rc=$?
check "--source-ip connects from each address" "[ $rc = 0 ] && j src.json 'd[\"metrics\"][\"total_connections_established\"] == 4 and d[\"metrics\"][\"error_rate\"] == 0' | grep -q True" "rc=$rc"
fi
$W $T -c 1 -d 1s --source-ip 192.0.2.7 --no-progress >srcbad.txt 2>&1; rc=$?
check "a foreign --source-ip is refused" "[ $rc = 1 ] && grep -q 'not usable on this machine' srcbad.txt" "rc=$rc"
$W ws://127.0.0.1:19033 --mode socketio --sio-event chat -c 5 -d 2s -r 10 --expect 'json:/0/seq' --no-progress --format json -o sio.json >/dev/null 2>&1; rc=$?
check "Socket.IO events are timed to their acks" "[ $rc = 0 ] && j sio.json '\"/socket.io/\" in d[\"target_url\"] and d[\"metrics\"][\"error_rate\"] == 0 and d[\"metrics\"][\"response_time\"][\"count\"] > 50' | grep -q True" "rc=$rc"
$W $T --mode socketio -c 1 -d 1s --message-timeout 300ms --no-reconnect --no-progress --format json -o siobad.json >/dev/null 2>&1
check "Socket.IO against a plain server times out opening" "j siobad.json 'd[\"metrics\"][\"error_breakdown\"].get(\"timeout\") == 1' | grep -q True" "$(j siobad.json "d['metrics']['error_breakdown']" 2>&1)"

# 0.2.4: permessage-deflate
$W mock --listen 127.0.0.1:19034 --compression >mock5.log 2>&1 &
$W mock --listen 127.0.0.1:19444 --compression --tls-cert c.pem --tls-key k.pem >mtls2.log 2>&1 &
sleep 0.5
Z=ws://127.0.0.1:19034
ZP='{"symbol":"ABC","bid":101.25,"ask":101.27,"size":300,"venue":"XNAS","side":"buy"}'
$W $Z --compression -c 5 -d 2s -p "$ZP" --expect 'json:/venue="XNAS"' --no-progress --format json -o z.json >/dev/null 2>&1; rc=$?
check "--compression is agreed and shrinks the wire" "[ $rc = 0 ] && j z.json 'd[\"metrics\"][\"compression\"][\"connections_negotiated\"] == 5 and d[\"metrics\"][\"compression\"][\"wire_bytes_recv\"] * 4 < d[\"metrics\"][\"compression\"][\"bytes_recv\"] == d[\"metrics\"][\"total_bytes_recv\"] and d[\"metrics\"][\"error_rate\"] == 0' | grep -q True" "rc=$rc $(j z.json 'd[\"metrics\"].get(\"compression\")' 2>&1)"
$W $Z --compression -c 2 -d 1s --no-progress >z.txt 2>&1
check "the summary has a Compression row" "grep -q 'permessage-deflate agreed on 2 of 2' z.txt" "$(grep -i compress z.txt)"
$W $T --compression -c 3 -d 1s --no-progress --format json -o zno.json >/dev/null 2>&1; rc=$?
check "a server that declines is served uncompressed" "[ $rc = 0 ] && j zno.json 'd[\"metrics\"][\"compression\"][\"connections_negotiated\"] == 0 and d[\"metrics\"][\"total_messages_recv\"] > 0' | grep -q True" "rc=$rc"
SSL_CERT_FILE=c.pem $W wss://127.0.0.1:19444 --compression -c 3 -d 1s -p "$ZP" --no-progress --format json -o ztls.json >/dev/null 2>&1; rc=$?
check "--compression over wss" "[ $rc = 0 ] && j ztls.json 'd[\"metrics\"][\"compression\"][\"connections_negotiated\"] == 3 and d[\"metrics\"][\"error_rate\"] == 0' | grep -q True" "rc=$rc"
$W $Z --compression -c 10 -d 2s -n 200 -p "$ZP" --no-progress --agents 127.0.0.1:17701,127.0.0.1:17702 --agent-token $TOK --format json -o zdist.json >/dev/null 2>&1; rc=$?
check "--compression across agents adds up" "[ $rc = 0 ] && j zdist.json 'd[\"metrics\"][\"compression\"][\"connections_negotiated\"] == 10' | grep -q True" "rc=$rc $(j zdist.json 'd[\"metrics\"].get(\"compression\")' 2>&1)"
sed "s#$T#$Z#" sc.yaml > zsc.yaml
$W --scenario zsc.yaml --compression --no-progress --format json -o zsc.json >/dev/null 2>&1; rc=$?
check "--compression in a scenario adds up its phases" "[ $rc = 0 ] && j zsc.json 'd[\"metrics\"][\"compression\"][\"connections_negotiated\"] == sum(p[\"metrics\"][\"compression\"][\"connections_negotiated\"] for p in d[\"phases\"]) >= 15' | grep -q True" "rc=$rc $(j zsc.json 'd[\"metrics\"].get(\"compression\")' 2>&1)"
if python3 -c 'import websockets' 2>/dev/null; then
cat > pyws.py <<'EOF'
import asyncio, sys
from websockets.asyncio.server import serve
async def echo(ws):
    try:
        async for m in ws:
            await ws.send(m)
    except Exception:
        pass
async def main():
    async with serve(echo, "127.0.0.1", int(sys.argv[1]), max_size=None):
        await asyncio.get_running_loop().create_future()
asyncio.run(main())
EOF
python3 pyws.py 19501 >pyws.log 2>&1 &
sleep 1
$W ws://127.0.0.1:19501 --compression -c 5 -d 2s -p "$ZP" --expect 'json:/size=300' --no-progress --format json -o zpy.json >/dev/null 2>&1; rc=$?
check "--compression against Python websockets" "[ $rc = 0 ] && j zpy.json 'd[\"metrics\"][\"compression\"][\"connections_negotiated\"] == 5 and d[\"metrics\"][\"error_rate\"] == 0' | grep -q True" "rc=$rc $(tail -3 pyws.log)"
cat > pycl.py <<'EOF'
import asyncio, os
from websockets.asyncio.client import connect
async def main():
    async with connect("ws://127.0.0.1:19034", max_size=None) as ws:
        assert ws.protocol.extensions, "no extension agreed"
        for m in ["hello " * 50, os.urandom(200000), "x" * 100000, '{"a":1}'] * 5:
            await ws.send(m)
            assert await ws.recv() == m
asyncio.run(main())
print("ok")
EOF
python3 pycl.py >pycl.log 2>&1
check "a Python websockets client against mock --compression" "grep -q '^ok' pycl.log" "$(tail -2 pycl.log)"
else
skip "--compression against Python websockets (pip install websockets)"
fi

# Socket.IO binary attachments
head -c 3000 /dev/urandom > blob.bin
$W ws://127.0.0.1:19033 --mode socketio --sio-namespace /files -c 3 -d 2s -r 20 --payload-file blob.bin --binary --no-progress --format json -o siobin.json >/dev/null 2>&1; rc=$?
check "Socket.IO binary events are acked with their attachment" "[ $rc = 0 ] && j siobin.json '(m:=d[\"metrics\"])[\"error_rate\"] == 0 and m[\"total_messages_recv\"] > 50 and m[\"total_bytes_recv\"] == 3000 * m[\"total_messages_recv\"]' | grep -q True" "rc=$rc"
if command -v node >/dev/null && npm install --silent --no-audit --no-fund socket.io@4 socket.io-client@4 >npm.log 2>&1; then
cat > sio-server.js <<'EOF'
const { Server } = require("socket.io");
const io = new Server(Number(process.argv[2]), { perMessageDeflate: { threshold: 0 }, maxHttpBufferSize: 1e8 });
for (const ns of ["/", "/files"]) {
  io.of(ns).on("connection", (socket) => {
    socket.on("chat", (msg, ack) => ack(Buffer.isBuffer(msg) ? msg : { ok: true, echo: msg }));
    socket.on("mixed", (msg, ack) => ack({ ok: true, size: msg.length, data: msg }));
    socket.emit("hello", Buffer.from([1, 2, 3]));
  });
}
EOF
node sio-server.js 19502 >sioserver.log 2>&1 &
sleep 1.5
$W ws://127.0.0.1:19502 --mode socketio --sio-event chat -c 3 -d 2s -p '{"n":{{seq}}}' --expect 'json:/0/ok=true' --compression --no-progress --format json -o siojs.json >/dev/null 2>&1; rc=$?
check "Socket.IO against a socket.io 4 server, compressed" "[ $rc = 0 ] && j siojs.json '(m:=d[\"metrics\"])[\"error_rate\"] == 0 and m[\"total_messages_recv\"] > 100 and m[\"total_unsolicited_frames\"] == 3 and m[\"compression\"][\"connections_negotiated\"] == 3' | grep -q True" "rc=$rc $(tail -2 sioserver.log)"
$W ws://127.0.0.1:19502 --mode socketio --sio-event chat --sio-namespace /files -c 3 -d 2s --payload-file blob.bin --binary --no-progress --format json -o siojsbin.json >/dev/null 2>&1; rc=$?
check "Socket.IO binary events against a socket.io 4 server" "[ $rc = 0 ] && j siojsbin.json '(m:=d[\"metrics\"])[\"error_rate\"] == 0 and m[\"total_messages_recv\"] > 100 and m[\"total_bytes_recv\"] == 3000 * m[\"total_messages_recv\"]' | grep -q True" "rc=$rc"
$W ws://127.0.0.1:19502 --mode socketio --sio-event mixed -c 2 -d 1s --payload-file blob.bin --binary --expect 'json:/0/size=3000' --no-progress --format json -o siojsmix.json >/dev/null 2>&1; rc=$?
check "a socket.io binary ack with JSON arguments is checked" "[ $rc = 0 ] && j siojsmix.json 'd[\"metrics\"][\"error_rate\"] == 0 and d[\"metrics\"][\"total_messages_recv\"] > 50' | grep -q True" "rc=$rc"
cat > sio-client.js <<'EOF'
const { io } = require("socket.io-client");
const s = io(process.argv[2] + "/files", { transports: ["websocket"] });
const ack = (...args) => new Promise((res, rej) => {
  const t = setTimeout(() => rej(new Error("no ack")), 2000);
  s.emit(...args, (...reply) => { clearTimeout(t); res(reply); });
});
s.on("connect", async () => {
  const blob = Buffer.alloc(5000, 7), second = Buffer.from("second");
  for (let i = 0; i < 20; i++) {
    const r = await ack("up", blob, { n: i }, second);
    if (!r[0].equals(blob) || r[1].n !== i || !Buffer.from(r[2]).equals(second)) { console.log("bad binary ack"); process.exit(1); }
    const t = await ack("chat", { n: i });
    if (t[0].n !== i) { console.log("bad ack"); process.exit(1); }
  }
  console.log("ok"); process.exit(0);
});
s.on("connect_error", (e) => { console.log("connect_error", e.message); process.exit(1); });
EOF
timeout 30 node sio-client.js http://127.0.0.1:19033 >sioclient.log 2>&1
check "a socket.io 4 client against mock --socketio" "grep -q '^ok' sioclient.log" "$(tail -2 sioclient.log)"
else
skip "Socket.IO against socket.io 4 (needs node and npm)"
fi

# Socket.IO 2 servers (Engine.IO 3)
$W ws://127.0.0.1:19033 --mode socketio --sio-version 2 --sio-namespace /chat -c 3 -d 2s -r 10 --expect 'json:/0/seq' --no-progress --format json -o sio2mock.json >/dev/null 2>&1; rc=$?
check "--sio-version 2 against mock --socketio" "[ $rc = 0 ] && j sio2mock.json '\"EIO=3\" in d[\"target_url\"] and d[\"metrics\"][\"error_rate\"] == 0 and d[\"metrics\"][\"total_messages_recv\"] > 30' | grep -q True" "rc=$rc"
if command -v node >/dev/null && mkdir -p sio2 && npm install --prefix sio2 --silent --no-audit --no-fund socket.io@2 socket.io-client@2 >npm2.log 2>&1; then
cat > sio2/server.js <<'EOF'
const io = require("socket.io")(Number(process.argv[2]), { pingInterval: 500, pingTimeout: 500, maxHttpBufferSize: 1e8 });
for (const ns of ["/", "/chat"]) {
  const nsp = io.of(ns);
  nsp.use((socket, next) => (ns === "/chat" && socket.handshake.query.token !== "t1" ? next(new Error("not authorized")) : next()));
  nsp.on("connection", (socket) => {
    socket.on("chat", (msg, ack) => ack(Buffer.isBuffer(msg) ? msg : { ok: true, echo: msg }));
    socket.emit("hello", Buffer.from([1, 2, 3]));
  });
}
EOF
node sio2/server.js 19503 >sio2server.log 2>&1 &
sleep 1.5
$W ws://127.0.0.1:19503 --mode socketio --sio-version 2 --sio-event chat -c 3 -d 3s -r 10 -p '{"n":{{seq}}}' --expect 'json:/0/ok=true' --no-progress --format json -o sio2js.json >/dev/null 2>&1; rc=$?
check "--sio-version 2 against a socket.io 2 server, past its ping timeout" "[ $rc = 0 ] && j sio2js.json '(m:=d[\"metrics\"])[\"error_rate\"] == 0 and m[\"total_reconnects\"] == 0 and m[\"total_messages_recv\"] > 60' | grep -q True" "rc=$rc $(tail -2 sio2server.log)"
$W ws://127.0.0.1:19503 --mode socketio --sio-version 2 --sio-namespace /chat --sio-auth '{"token":"t1"}' --sio-event chat -c 3 -d 2s --payload-file blob.bin --binary --no-progress --format json -o sio2bin.json >/dev/null 2>&1; rc=$?
check "--sio-version 2 binary events in a namespace with auth" "[ $rc = 0 ] && j sio2bin.json '(m:=d[\"metrics\"])[\"error_rate\"] == 0 and m[\"total_bytes_recv\"] == 3000 * m[\"total_messages_recv\"] > 0 and \"t1\" not in d[\"target_url\"]' | grep -q True" "rc=$rc"
$W ws://127.0.0.1:19503 --mode socketio --sio-version 2 --sio-namespace /chat --sio-auth '{"token":"no"}' -c 1 -d 1s --no-reconnect --no-progress --format json -o sio2deny.json >/dev/null 2>&1
check "a refused Socket.IO 2 namespace is a protocol_error" "j sio2deny.json 'd[\"metrics\"][\"error_breakdown\"].get(\"protocol_error\") == 1' | grep -q True" "$(j sio2deny.json "d['metrics']['error_breakdown']" 2>&1)"
cat > sio2/client.js <<'EOF'
const io = require("socket.io-client");
const s = io(process.argv[2] + "/files", { transports: ["websocket"], query: { token: "x" } });
const ack = (...args) => new Promise((res, rej) => {
  const t = setTimeout(() => rej(new Error("no ack")), 2000);
  s.emit(...args, (...reply) => { clearTimeout(t); res(reply); });
});
s.on("connect", async () => {
  const blob = Buffer.alloc(5000, 7);
  for (let i = 0; i < 20; i++) {
    const r = await ack("up", blob, { n: i });
    if (!Buffer.from(r[0]).equals(blob) || r[1].n !== i) { console.log("bad binary ack"); process.exit(1); }
  }
  console.log("ok"); process.exit(0);
});
s.on("error", (e) => { console.log("error", e); process.exit(1); });
EOF
timeout 30 node sio2/client.js http://127.0.0.1:19033 >sio2client.log 2>&1
check "a socket.io 2 client against mock --socketio" "grep -q '^ok' sio2client.log" "$(tail -2 sio2client.log)"
else
skip "Socket.IO against socket.io 2 (needs node and npm)"
fi

# TUI under a pseudo-terminal
if [ $LINUX = 1 ] && command -v script >/dev/null; then
script -qec "$W $T -c 5 -d 2s --tui" /dev/null >tui.txt 2>&1; rc=$?
check "TUI runs and exits cleanly" "[ $rc = 0 ]" "rc=$rc"
fi

# Subcommand order message
$W -c 2 mock >order.txt 2>&1; rc=$?
check "subcommand after flags explained" "[ $rc = 1 ] && grep -qi 'subcommand' order.txt" "$(cat order.txt | head -2)"

echo
echo "$PASS passed, $FAIL failed"
[ "$FAIL" = 0 ]
