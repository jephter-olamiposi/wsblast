# Measurement methodology

This document describes how wsblast generates load and how it computes every number in its reports. It describes the behaviour of the code in this repository, including its limits. If the code and this document disagree, the code is right and this document has a bug.

Contents:

1. [Execution model](#1-execution-model)
2. [Load models](#2-load-models)
3. [What is measured in each mode](#3-what-is-measured-in-each-mode)
4. [Timing](#4-timing)
5. [Histograms and percentiles](#5-histograms-and-percentiles)
6. [Counters, rates and formulas](#6-counters-rates-and-formulas)
7. [Connection accounting and reconnects](#7-connection-accounting-and-reconnects)
8. [Run duration and shutdown](#8-run-duration-and-shutdown)
9. [SLO evaluation and exit codes](#9-slo-evaluation-and-exit-codes)
10. [Known limitations](#10-known-limitations)

## 1. Execution model

- One process, one multi-threaded Tokio runtime.
- Each connection (`-c`) is one Tokio task, called a worker. A worker owns one WebSocket at a time and runs one session loop on it.
- Each worker records into its own metrics: two HDR histograms (handshake latency and message RTT) and plain integer counters. Nothing on the measurement path takes a lock.
- Workers also bump shared relaxed atomics. These feed only the progress bar and the TUI. The final report does not use them.
- When the run ends, every worker returns its metrics and the runner merges them into one report (`AggregatedMetrics::merge`).

## 2. Load models

### 2.1 Connection ramp

Workers are spawned in order. With `--ramp-rate R` (workers per second), worker `i` starts at `i / R` seconds. With `--ramp-rate 0` (the default) all workers start at once.

The ramp counts toward `--duration` (see [section 8](#8-run-duration-and-shutdown)). If `(connections - 1) / R` is at least the duration, wsblast prints a warning before the run, and the slots that were never started are reported as failed (see [section 7](#7-connection-accounting-and-reconnects)).

### 2.2 Message pacing

| Mode and rate | Model | Behaviour |
| :--- | :--- | :--- |
| `echo`, `--rate 0` | Closed loop | Send one message, wait for its reply (or `--message-timeout`), send the next one at once. Offered load is about `connections / RTT`, so it falls as the server slows down. |
| `echo`, `--rate R` | Paced closed loop | A per-connection `tokio::time::interval` with period `1/R`. Each tick sends one message and then waits for its reply. If a reply takes longer than `1/R`, the missed ticks are skipped (`MissedTickBehavior::Skip`), not sent late in a burst. A connection never has more than one request in flight. |
| `stream`, `--rate R` | Open loop per connection | An interval with period `1/R` and `MissedTickBehavior::Skip`. Sends do not wait for replies. Incoming frames are read in the same loop between sends. |
| `stream`, `--rate 0` | Open loop, timer driven | The same loop with a 10 µs interval. It is not a busy loop; in practice the timer and the socket writes bound the rate. |
| `listen` | No sending | The worker only reads frames (and answers pings). |
| `graphql-ws` | One subscription per session | `connection_init`, wait for `connection_ack`, one `subscribe` (or `start` for the legacy `graphql-ws` subprotocol), then read. |

`--rate` is per connection. The aggregate target rate is `connections x rate`. In a scenario, the phase's `rate_per_conn` plays the same role, and in an action script it paces every individual send.

The interval's first tick completes immediately, so a paced connection sends its first message as soon as the session starts.

### 2.3 Request cap

`-n, --requests N` caps the total messages sent across all workers. Each worker checks the shared sent counter before each send, and a watcher task checks it every 5 ms. When the cap is reached the run is cancelled. Because the check and the send are not one atomic step, concurrent workers can exceed the cap by up to about one message per connection. In a scenario the counter is shared by all phases, so the cap applies to the whole scenario: once it is reached, later phases are cancelled as soon as they start.

### 2.4 Keepalive pings

`--ping-interval D` (default `0s`, disabled) sends a WebSocket Ping frame with the payload `hb` every `D`. Pings and pongs are never counted as messages or bytes. In paced echo mode a keepalive tick that lands while the worker waits for its next send slot sends only the ping; it does not add a data send. In unpaced echo mode the keepalive timer is checked once per request without waiting. Scenario action scripts do not send keepalive pings; use a `Ping` action instead.

Every mode answers a server Ping with a Pong.

## 3. What is measured in each mode

| Mode | Messages sent | Messages received | RTT samples | Latency SLOs |
| :--- | :--- | :--- | :--- | :--- |
| `echo` | Each request written | Each reply recorded in the test window | One per reply | Allowed |
| `stream` | Each frame written | Every text or binary frame read | None | Rejected at startup |
| `listen` | 0 | Every text or binary frame read | None | Rejected at startup |
| `graphql-ws` | `connection_init` and `subscribe` (2 per session) | Each `next` or `data` message | None | Rejected at startup |

Only echo mode measures round-trip time. `--max-p50`, `--max-p95`, `--max-p99` and `--max-p999` with any other mode fail at startup with exit code 1:

```text
Error: Configuration error: Unsupported option combination: latency SLOs (--max-p50/--max-p95/--max-p99/--max-p999) require --mode echo; Stream mode does not measure round-trip time
```

Stream and listen mode count frames, not round trips. In stream mode against an echo server, received frames are the echoes, but they are not paired with requests.

In scenario action scripts (see [docs/scenarios.md](docs/scenarios.md)) the same rule holds: only echo mode waits for and times replies. In stream or listen mode an action script sends without waiting and does not read incoming frames, so messages received stays at 0.

### 3.1 How replies are matched (echo mode)

After a request is written, the worker reads frames until it finds the reply. Pings are answered and skipped, and pongs are skipped. A server Close frame ends the session; it is not counted as a reply and gets no RTT sample. A stream that ends without a Close frame is an `unexpected_close` error.

Which data frame counts as the reply is set by `--match`:

| `--match` | Reply |
| :--- | :--- |
| `first` (default) | The first text or binary frame received after the request, whatever its content. This is the behaviour of every earlier release. |
| `exact` | A frame whose payload equals the request. |
| `json:<pointer>` | A frame whose JSON value at the given JSON Pointer (RFC 6901) equals the request's value at the same pointer. |

`wsblast --help` lists the full rules for `exact` and `json:<pointer>`. With `first`, any unsolicited frame the server pushes (a broadcast, an ack, a welcome message) is taken as the reply to whatever request is in flight. Use one of the other modes against such servers.

If no reply arrives within `--message-timeout` (default `5s`), the worker records a `timeout` error and drops the connection. It then reconnects (see [section 7](#7-connection-accounting-and-reconnects)), so a late reply cannot be attributed to the next request.

## 4. Timing

- All intervals are measured with `std::time::Instant`, a monotonic clock. Wall-clock changes (NTP steps, DST) do not affect results.
- Only the `{{timestamp}}` payload placeholder uses the system clock (Unix time in milliseconds). It is never used for measurement.
- **RTT** starts immediately before the request is handed to the WebSocket sink and stops when the worker has read the reply frame. It includes client-side frame encoding and the socket write, the network in both directions, server processing, client-side frame decoding, and any delay before the Tokio scheduler polls the worker again. If the load generator's CPU is saturated, the scheduling delay inflates RTT.
- **Handshake latency** starts before `connect_async` and stops when the HTTP upgrade completes. It covers DNS resolution, TCP connect, the TLS handshake for `wss://`, and the HTTP upgrade. It is bounded by `--connect-timeout` (default `5s`); a handshake that takes longer is a `timeout` error.
- Durations are converted to whole microseconds by truncation and clamped to the histogram range (1 µs to 60 s) before recording. A sub-microsecond value is recorded as 1 µs.

## 5. Histograms and percentiles

### 5.1 Bounds and precision

Every histogram is `Histogram::<u64>::new_with_bounds(1, 60_000_000, 3)`:

| Property | Value |
| :--- | :--- |
| Unit | 1 µs |
| Lowest trackable value | 1 µs |
| Highest trackable value | 60,000,000 µs (60 s) |
| Precision | 3 significant figures |
| Exact resolution | 1 µs for values up to 2,047 µs |
| Resolution above that | Bucket width at most 0.1 % of the value (for example 8 µs around 10 ms) |
| Out-of-range values | Clamped into range before recording, never dropped |

Each worker has two histograms: handshake latency and message RTT. At the end of the run they are added into one combined histogram per metric. The combined histogram is a single distribution over all samples from all connections, so connections that completed more requests carry more weight.

### 5.2 Statistics

For `N` recorded samples and quantile `q`:

- `count` is `N`.
- `pXX` is computed as in HdrHistogram: take `k = max(1, ceil(q x N))`, walk the buckets in increasing order, and stop at the first bucket where the cumulative count reaches `k`. The reported value is the highest value that bucket can hold. The report includes p50, p75, p90, p95, p99, p99.9 (`p999_us`) and p99.99 (`p9999_us`).
- `max_us` is rounded up to its bucket's upper edge in the same way, and `min_us` is rounded down to its lower edge.
- `mean_us` and `std_dev_us` are computed from the midpoint of each bucket, weighted by its count.
- When `N` is 0, every field is 0. A latency SLO never treats this as a pass (see [section 9](#9-slo-evaluation-and-exit-codes)).

Because of bucket rounding, a percentile can be up to 0.1 % above the true sample value, and exact below 2,048 µs.

## 6. Counters, rates and formulas

| Field | Definition |
| :--- | :--- |
| `total_messages_sent` | Data frames written successfully (see the table in [section 3](#3-what-is-measured-in-each-mode)). |
| `total_messages_recv` | Data frames counted as received. |
| `total_bytes_sent`, `total_bytes_recv` | Payload bytes of those frames. WebSocket framing, masking, TLS records, the HTTP upgrade, pings and pongs are not counted. |
| `elapsed` | From just before the first worker is spawned to the moment the run is cancelled (see [section 8](#8-run-duration-and-shutdown)). |
| `throughput_msg_per_sec` | `max(total_messages_sent, total_messages_recv) / elapsed_secs` |
| `throughput_bytes_per_sec` | `(total_bytes_sent + total_bytes_recv) / elapsed_secs` |
| `error_rate` | `errors / (total_messages_sent + errors)`, where `errors` is the sum of `error_breakdown`. 0 when the denominator is 0. |

`elapsed_secs` has a floor of 0.0001 s to avoid division by zero.

Throughput uses the larger direction so that it is meaningful in every mode: listen mode never sends, and stream mode may send without replies. In echo mode sent and received differ by at most the requests in flight at the end of the run.

The progress bar and the TUI show a live "Msg/s" computed as received messages divided by elapsed time. It is a progress indicator and can differ from the final `throughput_msg_per_sec`.

### 6.1 What counts as an error

`error_breakdown` counts errors per category: `dns_resolution`, `tcp_connect`, `tls_handshake`, `http_upgrade_rejected`, `protocol_error`, `write_error`, `read_error`, `timeout`, `unexpected_close`, `other`.

- Every failed handshake attempt is one error, including each failed reconnect attempt. A target that stays down for the whole run therefore produces many errors per connection.
- In echo mode, each request that gets no reply within `--message-timeout` is one `timeout` error.
- A read or write failure on an established session is one error.
- In graphql-ws mode, `error` and `connection_error` messages are `protocol_error`.
- A server Close frame is not an error in any mode. In stream and listen mode a stream that ends without a Close frame is not recorded either; in echo mode it is `unexpected_close`.

### 6.2 Reading the error rate

The denominator counts sent messages plus errors, because a failed connection attempt sends nothing but must still lower the rate. Two consequences:

- A target that refuses every connection gives `error_rate = 1.0` (no messages, only errors).
- In echo mode a timed-out request is counted once as sent and once as an error. If every request times out, the error rate is 0.5, not 1.0. A local run against a server that replies after 50 ms, with `--message-timeout 20ms`, reported 51 messages sent, 51 `timeout` errors and `error_rate = 0.5`.

`--max-error-rate` takes a fraction between 0.0 and 1.0 and passes when `error_rate <= threshold`.

## 7. Connection accounting and reconnects

Connection counts are per worker slot, not per handshake:

| Field | Definition |
| :--- | :--- |
| `total_connections_attempted` | Worker slots the run was configured to open: `-c`, or the sum over scenario phases. |
| `total_connections_established` | Slots that completed at least one handshake. |
| `total_connections_failed` | `attempted - established`: slots that never completed a handshake. |
| `total_reconnects` | Extra handshakes that re-established a slot after it lost its connection. |

`handshake_latency.count = total_connections_established + total_reconnects`.

A slot that was never started, because the ramp could not reach it or a request cap ended the run first, counts as failed even though no error was recorded for it. A local run with `-c 10 --ramp-rate 2 -d 2s` reported 10 attempted, 4 established, 6 failed and an empty `error_breakdown`.

### 7.1 Reconnect policy

A worker reconnects when a handshake fails or an established session is lost (server Close, timeout, read or write error), as long as the run is still going.

- Backoff starts at 100 ms and doubles on each consecutive failure (100, 200, 400, 800, 1600, 3200 ms) up to a cap of 5 s.
- A successful handshake resets the backoff to 100 ms. So after a lost session the first retry waits 100 ms.
- Cancellation interrupts the backoff wait.
- Each successful reconnect adds one sample to `handshake_latency` and one to `total_reconnects`. Each failed attempt adds one error but does not change the slot counts.
- Payload sequence numbers (`{{seq}}`) restart at 0 on each new session.

`--no-reconnect` ends the worker at the first lost session or failed handshake. Its slot stays idle for the rest of the run. In a scenario, a `Close` action also ends the worker without reconnecting.

If the handshake request cannot be built from the configuration at all, the slot records one `other` error and is not retried.

## 8. Run duration and shutdown

### 8.1 Duration

`--duration` is the total wall-clock time of the run, including the connection ramp. One timer starts just before the first worker is spawned, and it is the same start point used for `elapsed`, throughput and the progress bar. A run does not last "ramp plus duration".

The run is cancelled by the first of:

- the `--duration` timer,
- the `--requests` cap,
- Ctrl+C or SIGTERM (Ctrl+C only on Windows),
- `q`, `Esc` or Ctrl+C in the TUI.

`elapsed` is read when the runner sees the cancellation. With `--no-progress` (or `--tui`) that is immediate. With the progress bar, the runner polls every 100 ms, so `elapsed` can exceed `--duration` by up to about 100 ms; local runs with `-d 1s` reported 1.011 to 1.012 s with the progress bar and 2.0007 s for `-d 2s` with `--no-progress`.

In a scenario each phase has its own duration and `elapsed`. The report's `elapsed` is the sum of the phases' values; the 500 ms pause between phases is not included.

### 8.2 Shutdown semantics

On cancellation each worker stops at its next await point:

- In echo mode, a send that has not completed is abandoned and not counted. In stream mode a send that has already started runs to completion and is counted.
- In echo mode, a request whose reply has not arrived is counted as sent but not as received, and gets no RTT sample. Cancellation is checked before a reply that is ready at the same moment, so nothing that arrives after the test window is recorded.
- The worker sends a Close frame (code-less) and exits. It does not reconnect.

So in echo mode `total_messages_sent - total_messages_recv` is at most the number of connections on a clean run. A 2 s run with 4 connections reported 151,471 sent and 151,468 received.

Ctrl+C and SIGTERM stop the run the same way the timer does: the report is printed and the exit code follows the normal rules. During a scenario they stop the current phase and skip the remaining phases.

### 8.3 Timer resolution

Pacing, ramp, think time and `Wait`/`Ping` actions all use Tokio timers, which have millisecond granularity. Per-connection rates above about 1000 msg/s are therefore approximate. In local runs on Linux with one connection for 2 s:

| `--rate` | echo, achieved msg/s | stream, achieved msg/s |
| ---: | ---: | ---: |
| 500 | 499 | 500 |
| 1000 | 995 | 998 |
| 2000 | 1978 | 1958 |
| 5000 | 4846 | 4970 |

These numbers depend on the machine, the OS and its load. For a high aggregate rate, prefer more connections at a moderate per-connection rate.

## 9. SLO evaluation and exit codes

SLOs are evaluated once, on the final merged metrics.

| Flag | Check | Passes when |
| :--- | :--- | :--- |
| `--max-p50 D` | `message_rtt.p50_us` | `p50 <= D` |
| `--max-p95 D` | `message_rtt.p95_us` | `p95 <= D` |
| `--max-p99 D` | `message_rtt.p99_us` | `p99 <= D` |
| `--max-p999 D` | `message_rtt.p999_us` | `p99.9 <= D` |
| `--max-error-rate F` | `error_rate` | `error_rate <= F` |
| `--min-throughput X` | `throughput_msg_per_sec` | `throughput >= X` |

A latency check with no RTT samples (for example every request timed out) fails with the actual value "no RTT samples". It never passes on an empty histogram.

| Exit code | Meaning |
| :--- | :--- |
| 0 | The run completed, every configured SLO passed (or none were set), and at least one connection was established. |
| 1 | Configuration or runtime error (invalid URL, scheme or duration, latency SLO outside echo mode, invalid scenario, unreadable file, report write failure), or every connection slot failed and no SLO failed. |
| 2 | At least one SLO failed. Also used by `--compare --fail-on-regression` when a regression is found, and by the argument parser for invalid or conflicting flags. |

SLOs are checked before the "all connections failed" condition. A run where every connection fails and an SLO also fails (for example `--max-error-rate 0.5`) exits with 2, not 1. The report is always printed (and written with `-o`) before the process exits with 1 or 2 after a completed run.

## 10. Known limitations

- **No coordinated-omission correction.** Both echo load models are closed loop: a connection waits for each reply before it sends again, and missed pacing ticks are skipped. When the server stalls for a time `T`, each connection records one slow sample instead of the `T x rate` samples a constant-rate client would have seen. Percentiles therefore understate tail latency under stalls, and the effect grows with the stall length and the configured rate. Correction is planned for v0.3.0. Until then, compare runs made with the same settings, and treat high percentiles as a lower bound when the server has pauses.
- **Memory per connection.** Each connection holds two histograms of about 136 KiB each, so about 270 KB per connection before any socket or TLS buffers. 10,000 connections need about 2.7 GB for histograms alone. A local echo run with 1,000 connections at `--rate 1` peaked at about 420 MB resident.
- **Reply matching.** Echo replies are matched by `--match` (default `first`, see [section 3.1](#31-how-replies-are-matched-echo-mode)). With `first`, unsolicited server frames are taken as replies.
- **Single generator.** All load comes from one process. wsblast does not detect when the generator itself is the bottleneck (CPU saturation, file descriptor limit, ephemeral ports). It prints a warning when `-c` plus 64 exceeds the open file limit (`ulimit -n`). Watch the generator's CPU when RTT matters.
- **Timer resolution.** Per-connection rates above about 1000 msg/s are approximate (see [section 8.3](#83-timer-resolution)).
- **Payload bytes only.** Byte counts exclude framing, TLS and handshake traffic, so they understate wire bandwidth.
- **Request cap overshoot.** `--requests` can be exceeded by up to about one message per connection.
- **Slot-based failure count.** `total_connections_failed` includes slots that were never started. Read it together with the ramp warning and `error_breakdown`.
- **Scenario report header.** In a scenario, the report's `target_url` is the first executed phase's target, even when later phases used other targets.
