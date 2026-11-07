# Mock server

`wsblast mock` runs a `ws://` test target on your machine, or a `wss://` one with a certificate. By default it echoes every text and binary frame back. Flags add the failure modes a real server has, so you can see what each one does to a wsblast report, and test a CI gate against a server whose behaviour you know in advance. With `--replay` it plays back the traffic of a real server recorded with `--capture`, and with `--rules` it answers like a real API: matched requests get templated replies, their own delay or a close.

It is part of the default build (Cargo feature `mock`). The same server is available to Rust tests as [`wsblast::mock`](https://docs.rs/wsblast/latest/wsblast/mock/index.html).

Contents:

1. [Flags](#1-flags)
2. [How each behaviour works](#2-how-each-behaviour-works)
3. [Output and exit codes](#3-output-and-exit-codes)
4. [Examples](#4-examples)
5. [Recording and replaying a real server](#5-recording-and-replaying-a-real-server)
6. [Testing a CI gate against the mock](#6-testing-a-ci-gate-against-the-mock)
7. [Using the mock from Rust tests](#7-using-the-mock-from-rust-tests)
8. [Serving wss:// (TLS)](#8-serving-wss-tls)
9. [Scripted replies (`--rules`)](#9-scripted-replies---rules)

## 1. Flags

| Flag | Default | Effect |
| :--- | :--- | :--- |
| `--listen ADDR` | `127.0.0.1:9001` | Address to listen on. Port 0 picks a free port; the startup line shows which. |
| `--delay DURATION` | `0s` | Hold every reply for this long after its request arrived. |
| `--jitter DURATION` | `0s` | Add a uniformly distributed extra delay between 0 and this to every reply. |
| `--close-after N` | off | Close each connection with code 1000 after it sent N replies. |
| `--reject-ratio F` | `0` | Answer this fraction (0.0 to 1.0) of upgrade requests with HTTP 503. |
| `--stall-every DURATION` | off | Stop replying on every connection at each multiple of this period. Needs `--stall-for`. |
| `--stall-for DURATION` | off | How long each stall lasts. Must be shorter than `--stall-every`. |
| `--replay FILE` | off | Instead of echoing, send each connection the frames a `--capture` file recorded as received. |
| `--rules FILE` | off | Answer frames as a YAML rules file says: match by text or JSON field, reply from templates, delay or close. Cannot be combined with `--replay`. See [section 9](#9-scripted-replies---rules). |
| `--socketio` | off | Speak Socket.IO (Engine.IO 4): accept any namespace, acknowledge every event that asks for an ack with its arguments after the reply delay, echo the others, and ping every 25 s, closing a connection that misses its pong for 20 s. Cannot be combined with `--rules` or `--replay`. See [docs/socketio.md](socketio.md#5-testing-against-the-mock). |
| `--compression` | off | Accept permessage-deflate when a client offers it: inflate its messages before answering them and compress the replies. Clients that do not offer it are served uncompressed. The stop line counts the compressed connections. See [section 2](#2-how-each-behaviour-works). |
| `--tls-cert PEM` | off | Serve `wss://` with this PEM certificate chain. Needs `--tls-key`. See [section 8](#8-serving-wss-tls). |
| `--tls-key PEM` | off | PEM private key (PKCS#8, PKCS#1 or SEC1) for `--tls-cert`. |

Durations take the same units as wsblast's other duration flags: `ns`, `us`, `ms`, `s`, `m`, `h`, fractions allowed (`1.5ms`), or a bare integer number of seconds. All behaviours combine. An invalid value (a ratio outside 0 to 1, `--close-after 0`, a stall that is not shorter than its period, a file that is not a capture, a certificate or key that cannot be read, is not PEM or does not match) prints an error and exits with code 1 before the server starts.

## 2. How each behaviour works

**Echo.** Every text or binary frame is sent back unchanged, as the same frame type, on the connection it arrived on. Replies keep the order of their requests. Pings are answered with pongs by the WebSocket library and are not counted. When the client sends a Close frame, the server completes the close handshake and ends the connection.

**Delay and jitter.** A frame that arrives at time `t` is due at `t + delay + j`, where `j` is drawn for each reply, uniformly from 0 to `--jitter` inclusive, with a xorshift64 generator. The server reads continuously while replies wait, so a client that sends several frames without waiting (stream mode) has each one delayed from its own arrival, not from the previous reply. Because replies keep their order, a reply that drew a small jitter can wait behind an earlier one that drew a large jitter. The delay is a floor: timers never fire early, so an echo round trip is never shorter than `--delay`. They can fire late, by up to about 1 ms on Linux and macOS and about 15.6 ms on Windows. Without `--delay` and `--jitter`, no timer is involved: a reply goes out as soon as its frame is read.

**Close after N.** After its N-th reply (echoed or replayed) the server sends a Close frame with code 1000 and reason `close-after` and waits up to 2 s for the client to complete the close. A wsblast worker then reconnects with its usual backoff (100 ms, doubling up to 5 s, reset after a successful handshake), so the run's `Reconnects` counter goes up by one for every closed session. With `--no-reconnect` the worker stops instead. The close is not an error for wsblast: in echo mode, the request that was in flight is counted as sent and not received.

**Reject ratio.** For every TCP connection the server draws a number in [0, 1) before reading the upgrade request; when it is below `--reject-ratio`, the upgrade is answered with `HTTP 503 Service Unavailable` and the connection is closed. `0` never rejects and `1` rejects everything. wsblast counts each rejection as one `http_upgrade_rejected` error and one failed handshake, and retries the slot with backoff unless `--no-reconnect` is set, so a ratio of 0.3 leaves about 30 % of the handshake attempts failing, not 30 % of the slots.

**Stalls.** Stalls are server-wide and periodic, counted from the moment the server started: with `--stall-every E --stall-for F`, the server is stalled during `[k·E, k·E + F)` for every k ≥ 1. A reply that becomes due during a stall is held until the stall ends, on every connection, and then sent with the replies queued behind it. Requests are still read during a stall. Nothing is dropped. This is what a garbage-collection pause or a blocked event loop looks like from the outside, and it is the case wsblast's response time exists for: in a paced run (`--rate`), every request that falls due during a stall waits behind the held one, and response time counts that wait while RTT records one slow sample per stall. See [METHODOLOGY.md, section 2.2](../METHODOLOGY.md#22-message-pacing).

**Compression.** With `--compression` the server accepts a `permessage-deflate` offer with context takeover on both sides, answering `server_no_context_takeover` when the client asked for it, and declines an offer that asks for a server window below 32 KiB. Every reply on an accepted connection is compressed, and every behaviour above (delay, stalls, rules, replay, Socket.IO) works on the inflated messages. It is how wsblast's own `--compression` is tested, and a way to measure what compression costs a client.

**Replay.** See [section 5](#5-recording-and-replaying-a-real-server).

## 3. Output and exit codes

The server writes to stdout: one line at startup with the address and the active behaviours,

```text
wsblast mock listening on ws://127.0.0.1:9001 (echo, delay 20ms, jitter 0..5ms, stall 200ms every 2s)
```

then, every second in which anything changed or a connection was open, one stats line, with the counters since startup and the frames per second over the last second,

```text
[stats] connections: 50 active, 50 accepted, 0 rejected | frames: 412339 received, 412289 sent | rate: 41190 recv/s, 41188 sent/s
```

and a summary when it stops. `accepted` counts completed WebSocket handshakes and `rejected` the upgrades answered with 503; frames are text and binary frames only.

Ctrl+C or SIGTERM (Ctrl+C on Windows) stops the server: open connections get a Close frame with code 1001 (going away), the server waits up to 2 s for them to finish, and the process exits with code 0. A configuration error exits with 1, as does an address that cannot be bound.

## 4. Examples

```bash
# A plain echo server on the default address.
wsblast mock

# A server with 20 ms of latency and up to 10 ms of jitter.
wsblast mock --delay 20ms --jitter 10ms
wsblast ws://127.0.0.1:9001 -c 20 -d 10s

# Connections that drop every 100 replies: watch the Reconnects counter.
wsblast mock --close-after 100
wsblast ws://127.0.0.1:9001 -c 10 -d 10s

# A flaky gateway that refuses a quarter of the upgrades.
wsblast mock --reject-ratio 0.25
wsblast ws://127.0.0.1:9001 -c 50 -d 10s --max-error-rate 0.01   # exits with 2

# A 300 ms pause every 2 s. Compare the RTT and Response Time columns.
wsblast mock --stall-every 2s --stall-for 300ms
wsblast ws://127.0.0.1:9001 -c 10 -r 100 -d 20s
```

In the last example only one request per connection and stall has a slow round trip, 0.5 % of them, so the RTT p99 stays at the normal round-trip time while the RTT p99.9 shows the stall. Response time counts every request that fell due during a stall, about 15 % of them, so its p99 is close to the full 300 ms: that is what users of a stalling server experience. A local run measured an RTT p99 of 3 ms and a response time p99 of 287 ms.

## 5. Recording and replaying a real server

Replay turns traffic recorded from a real server into a local, repeatable target, for example to reproduce a burst of server pushes in a test.

1. Record a run against the real server with `--capture` ([METHODOLOGY.md, section 11](../METHODOLOGY.md#11-frame-capture)). Every worker's received frames are recorded with microsecond timestamps.

   ```bash
   wsblast wss://api.example.com/feed --mode listen -c 3 -d 30s --capture feed.jsonl
   ```

2. Replay the recording locally:

   ```bash
   wsblast mock --replay feed.jsonl
   ```

3. Point the load test at the mock:

   ```bash
   wsblast ws://127.0.0.1:9001 --mode listen -c 3 -d 30s
   ```

How replay works:

- The file must start with a `wsblast_capture` version 1 header. Only `recv` records of kind `text` and `binary` are replayed; sent frames, `open` records and Close frames are not. Binary frames are decoded from base64.
- Records are grouped by their `worker` field, and in a capture of a distributed run ([docs/distributed.md](distributed.md#4-scenarios-capture-refreshed-tokens-and-live-export)) by their `agent` and `worker` fields, since every agent numbers its connections from 0. Each recorded worker's frames are one script, with each frame's time taken relative to that worker's first record of any kind (normally its `open` record). Workers that received no data frames are skipped.
- Connections are assigned scripts round robin in the order their handshakes complete: the first connection gets the lowest recorded worker number (of the lowest agent), the next one the next worker, and so on, wrapping around. Rejected upgrades do not take a turn.
- Each frame is sent at the connection's start plus its recorded offset, plus `--delay` and a fresh jitter draw if set. A stall holds replayed frames like echo replies, and `--close-after N` closes the connection after N replayed frames.
- Frames from the client are read, counted and discarded. After the script's last frame the connection stays open until the client closes it or the server stops.
- A worker that reconnected during the recording has all its sessions in one script, so its replay contains the frames of every session, spaced as they were recorded.

## 6. Testing a CI gate against the mock

Before trusting a gate on a real service, check that it passes and fails when it should, against a server with known latency. The step below starts the mock with a 20 ms delay, then checks that a 100 ms budget passes (exit code 0) and a 5 ms budget fails (exit code 2):

```yaml
- name: Check the SLO gate against the mock
  shell: bash
  run: |
    wsblast mock --listen 127.0.0.1:9001 --delay 20ms &
    mock_pid=$!
    trap 'kill "$mock_pid" 2>/dev/null || true' EXIT
    for _ in $(seq 1 50); do
      (exec 3<>/dev/tcp/127.0.0.1/9001) 2>/dev/null && break
      sleep 0.2
    done

    expect() {
      local want="$1"; shift
      set +e
      wsblast ws://127.0.0.1:9001 -c 10 -d 3s --no-progress "$@"
      local code=$?
      set -e
      if [ "$code" -ne "$want" ]; then
        echo "::error::wsblast $* exited with $code, expected $want"
        exit 1
      fi
    }

    expect 0 --max-p99 100ms
    expect 2 --max-p99 5ms
    # A stalling server must fail a response-time gate.
    kill "$mock_pid"; wait "$mock_pid" || true
    wsblast mock --listen 127.0.0.1:9001 --stall-every 1s --stall-for 200ms &
    mock_pid=$!
    sleep 1
    expect 2 -r 50 --max-p99 100ms
```

The same pattern tests error-rate gates with `--reject-ratio` and reconnect handling with `--close-after`. Keep budgets generous on shared runners: the mock's delay is a floor, not an exact value (see [section 2](#2-how-each-behaviour-works)).

## 7. Using the mock from Rust tests

The server is a library API, so integration tests can run it in-process on a free port:

```rust
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use wsblast::mock::{MockConfig, MockServer};
use wsblast::{LoadTestConfig, Runner};

#[tokio::test]
async fn gate_catches_a_slow_server() {
    let config = MockConfig::builder()
        .listen("127.0.0.1:0")
        .delay(Duration::from_millis(20))
        .build()
        .unwrap();
    let server = MockServer::bind(config).await.unwrap();
    let url = server.url();
    let stats = server.stats();
    let cancel = CancellationToken::new();
    let serving = tokio::spawn(server.run(cancel.clone()));

    let load = LoadTestConfig::builder(url).connections(4).build().unwrap();
    let metrics = Runner::new(load).with_signal_handling(false).run().await.unwrap();
    assert!(metrics.message_rtt.min_us >= 20_000);
    assert_eq!(stats.snapshot().connections_accepted, 4);

    cancel.cancel();
    serving.await.unwrap().unwrap();
}
```

`MockConfigBuilder::seed` fixes the jitter and rejection draws for repeatable tests; the command line always seeds from the clock. `wsblast::mock::serve(config, cancel)` binds and runs in one call when the address is fixed.

`MockConfigBuilder::tls(cert, key)` is the library equivalent of `--tls-cert` and `--tls-key`; `MockServer::url` then returns a `wss://` URL.

## 8. Serving wss:// (TLS)

With `--tls-cert` and `--tls-key` the mock completes a TLS handshake (rustls, TLS 1.2 and 1.3, the same server code as `wsblast agent --tls-cert`) before the WebSocket upgrade, and the startup line shows a `wss://` URL. Every behaviour works the same over TLS. A client whose TLS handshake fails or does not finish within 10 s is disconnected and not counted as accepted or rejected.

The mock does not generate certificates. Create one for `localhost` with either tool:

```bash
# mkcert: a certificate signed by a local CA that mkcert adds to the system
# and browser trust stores (run `mkcert -install` once).
mkcert -cert-file mock-cert.pem -key-file mock-key.pem localhost 127.0.0.1 ::1

# OpenSSL 3: a self-signed certificate, trusted by nothing until you say so.
openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes \
  -keyout mock-key.pem -out mock-cert.pem -days 30 -subj "/CN=localhost" \
  -addext "subjectAltName=DNS:localhost,IP:127.0.0.1,IP:::1" \
  -addext "basicConstraints=critical,CA:FALSE"
```

Keep the `basicConstraints=critical,CA:FALSE` line: `openssl req -x509` marks its certificates as CAs by default, and rustls refuses a CA certificate as a server certificate, so without it every handshake fails with `tls_handshake` errors even when the certificate is trusted.

Then serve and test, pointing wsblast at the certificate it should trust. wsblast reads its roots from the platform's store, and `SSL_CERT_FILE` replaces that store with a PEM file (`SSL_CERT_DIR` with a directory), so the self-signed certificate itself can be the only trusted root:

```bash
wsblast mock --tls-cert mock-cert.pem --tls-key mock-key.pem
SSL_CERT_FILE=mock-cert.pem wsblast wss://localhost:9001 -c 10 -d 10s
```

Connect with a name the certificate lists. The mock's startup line shows the address it bound (`wss://127.0.0.1:9001`). Both commands above list `127.0.0.1` and `::1` as well as `localhost`; a certificate issued only for `localhost` fails verification at an IP address, which wsblast reports as `tls_handshake` errors. With mkcert's CA installed, `SSL_CERT_FILE` is not needed.

## 9. Scripted replies (`--rules`)

A rules file makes the mock answer like the API you are testing instead of echoing: a subscription gets an acknowledgement carrying its id, a `ping` gets a `pong`, and a connection closes after its hundredth message. There is no scripting language: a rule matches a frame, and says what to send back and when.

```yaml
version: "1.0"
rules:
  # A JSON request whose "type" is "subscribe" gets an ack with its id,
  # then a data frame, both 20 ms later.
  - match: { json: /type, equals: subscribe }
    reply:
      - '{"type":"ack","id":{{json:/id}}}'
      - '{"type":"data","seq":{{seq}}}'
    delay: 20ms
  # The text frame "ping" gets "pong".
  - match: { text: ping }
    reply: pong
  # From the 100th frame on, the connection closes with code 4000.
  - after: 100
    close: { code: 4000, reason: done }
# What a frame no rule matches gets: echo (the default) or ignore.
unmatched: echo
```

```bash
wsblast mock --rules rules.yaml
```

**Matching.** Rules are tried in order and the first one that applies decides; later rules are not looked at. A rule applies when:

| Key | Meaning |
| :--- | :--- |
| `match: { text: T }` | The frame is a text frame equal to `T`. |
| `match: { json: /pointer }` | The frame is JSON (text, or UTF-8 binary) with a value at the [RFC 6901](https://www.rfc-editor.org/rfc/rfc6901) pointer. |
| `match: { json: /pointer, equals: V }` | The same, and the value equals `V`, compared as JSON: `equals: 7` matches the number 7, `equals: "7"` the string. |
| no `match` | Every text and binary frame. |
| `after: N` | Only from the connection's `N`-th received frame on, counting from 1. Combines with `match`. |

**What a rule does.** A rule needs `reply`, `close` or both:

| Key | Meaning |
| :--- | :--- |
| `reply` | One template, or a list sent in order as text frames. `reply: []` sends nothing, to swallow a frame. |
| `delay` | Hold this rule's replies for this long instead of `--delay`. `--jitter` still adds to it. |
| `close` | After the replies, close the connection: `close: 4000`, or `close: { code: 4000, reason: done }`. Codes 1000-1003, 1007-1014 and 3000-4999 can be sent; the reason is at most 123 bytes. |

**Templates.** A reply is literal text with these placeholders:

| Placeholder | Becomes |
| :--- | :--- |
| `{{json:/pointer}}` | The request's JSON value at the pointer, written as JSON: a string keeps its quotes, so `{"id":{{json:/id}}}` stays valid JSON for numeric and string ids. `null` when the request has no such value or is not JSON. |
| `{{seq}}` | The request's number on its connection, from 1. |
| `{{request}}` | The request itself, as text. |

Any other `{{...}}` is an error when the file is loaded, so a typo cannot silently send a literal placeholder.

**Timing.** All replies of one rule are due at the same time: the rule's `delay` (or `--delay`) plus jitter after the request arrived. As with echoing, replies keep the order of their requests, stalls hold them back, `--close-after` counts every reply frame, and `--reject-ratio` still applies to upgrades.

**Validation.** The file is checked when the mock starts. A missing `version: "1.0"`, an unknown key, a pointer that does not start with `/`, `after: 0`, a close code that cannot be sent or an unknown placeholder is an error that names the rule (`rule 2: close code 1005 cannot be sent`), and the mock exits with code 1 before listening.

**Correlating replies in a load test.** The client side of the first example is `--match json:/id` with a payload that carries `{{seq}}`, so every request is timed to the reply that echoes its id, and frames the server pushes in between (the data frames above) are skipped and counted as unsolicited:

```bash
wsblast mock --rules rules.yaml &
wsblast ws://127.0.0.1:9001 -c 50 -d 30s -r 10 \
  -p '{"type":"subscribe","id":{{seq}}}' --match json:/id
```

From Rust, [`MockConfigBuilder::rules`](https://docs.rs/wsblast/latest/wsblast/mock/struct.MockConfigBuilder.html#method.rules) loads the same file.
