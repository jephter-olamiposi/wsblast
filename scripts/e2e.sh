#!/bin/bash
# End-to-end checks of every wsblast feature, run against the real binary as
# a user would: exit codes, report formats, scenarios, capture and replay,
# exports, comparison, chaos, authentication, TLS, distributed runs, data
# files, reply checks, named actions, arrival rates, source addresses and
# Socket.IO. Every server it needs is started locally (wsblast mock, agents,
# small Python stand-ins for an OTLP collector and an OAuth2 token endpoint).
#
#   scripts/e2e.sh [path/to/wsblast]
#
# The binary must be built with every feature:
#   cargo build --release --all-features && scripts/e2e.sh target/release/wsblast
# Needs bash, python3, curl and openssl; the source-address and TUI checks
# run on Linux only. Uses ports 19001-19033, 19443, 19464, 14318, 14444 and
# 17701-17703 on 127.0.0.1. Exits 1 when any check fails.
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
check "response time p95 > RTT p95 under stalls" "j co.json 'd[\"metrics\"][\"response_time\"][\"p95_us\"] > d[\"metrics\"][\"message_rtt\"][\"p95_us\"]' | grep -q True" "$(j co.json 'd[\"metrics\"][\"response_time\"][\"p95_us\"], d[\"metrics\"][\"message_rtt\"][\"p95_us\"]' 2>&1)"
$W $T -c 40 -n 101 -d 10s --no-progress --format json -o n.json >/dev/null 2>&1
check "-n 101 sends exactly 101" "[ \"\$(j n.json 'd[\"metrics\"][\"total_messages_sent\"]')\" = 101 ]" "$(j n.json 'd[\"metrics\"][\"total_messages_sent\"]')"

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
check "distributed -n is exact across agents" "[ \"\$(j dist.json 'd[\"metrics\"][\"total_messages_sent\"]')\" = 300 ]" "$(j dist.json 'd[\"metrics\"][\"total_messages_sent\"]')"
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
check "Socket.IO against a plain server times out opening" "j siobad.json 'd[\"metrics\"][\"error_breakdown\"].get(\"timeout\") == 1' | grep -q True" "$(j siobad.json 'd[\"metrics\"][\"error_breakdown\"]' 2>&1)"

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
