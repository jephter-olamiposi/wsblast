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
- Each worker records into its own metrics: HDR histograms (handshake latency, message RTT and, for paced echo, response time) and plain integer counters. Nothing on the measurement path takes a lock.
- Workers also bump relaxed atomic counters that feed the progress bar, the TUI and the time series ([section 6.3](#63-time-series)). They are split into cache-line-aligned shards, twice as many as the machine has CPU cores, and workers are assigned to shards round robin, so workers on different cores rarely write to the same cache line. In a C benchmark on a 4-core machine, three relaxed adds per message cost about 138 ns on shared counters and about 4 ns on per-thread shards. The final report's totals come from the per-worker metrics, not from these counters.
- When the run ends, every worker returns its metrics and the runner merges them into one report (`AggregatedMetrics::merge`).

## 2. Load models

### 2.1 Connection ramp

Workers are spawned in order. With `--ramp-rate R` (workers per second), worker `i` starts at `i / R` seconds. With `--ramp-rate 0` (the default) all workers start at once.

The ramp counts toward `--duration` (see [section 8](#8-run-duration-and-shutdown)). If `(connections - 1) / R` is at least the duration, wsblast prints a warning before the run, and the slots that were never started are reported as not started (see [section 7](#7-connection-accounting-and-reconnects)).

### 2.1.1 Load stages

`--stages D1:N1,D2:N2,...` replaces `-c`, `-d` and `--ramp-rate` with a load profile. Over stage `i`, the target number of open connections moves linearly from `N(i-1)` (0 before the first stage) to `Ni` over `Di`, and stays at the last target after the last stage. Every 10 ms the runner compares the number of running workers with the target rounded to the nearest connection, starts new workers when it is below and stops the most recently started workers when it is above. A stopped worker closes its connection with a normal WebSocket close; one stopped during its first handshake counts as interrupted.

The run lasts the sum of the stage durations, `connections` in the report is the highest target, and `total_connections_attempted` counts every worker the run started, so a profile that ramps down and up again (`10s:100,10s:20,10s:100`) attempts more connections than its peak (180 here). No slot is reported as not started. The time series' `active_connections` shows the profile the run achieved; with `--rate`, its throughput follows the profile too. `--stages` cannot be used with `--scenario`, whose phases set their own connections and ramp.

### 2.2 Message pacing

| Mode and rate | Model | Behaviour |
| :--- | :--- | :--- |
| `echo`, `--rate 0` | Closed loop | Send one message, wait for its reply (or `--message-timeout`), send the next one at once. Offered load is about `connections / RTT`, so it falls as the server slows down. |
| `echo`, `--rate R` | Paced closed loop on a fixed schedule | Request `n` of a session is due at `session start + n/R`. A request is never sent before it is due. When a slow reply makes the next request late, it is sent as soon as that reply arrives, so the connection keeps the configured rate on average for as long as the server keeps up. A connection never has more than one request in flight. Response time is recorded for each reply ([section 4](#4-timing)). |
| `stream`, `--rate R` | Open loop per connection | An interval with period `1/R` and `MissedTickBehavior::Skip`. Sends do not wait for replies. Incoming frames are read in the same loop between sends. |
| `stream`, `--rate 0` | Open loop, timer driven | The same loop with a 10 µs interval. It is not a busy loop; in practice the timer and the socket writes bound the rate. |
| `listen` | No sending | The worker only reads frames (and answers pings). |
| `graphql-ws` | One subscription per session | `connection_init`, wait for `connection_ack`, one `subscribe` (or `start` for the legacy `graphql-ws` subprotocol), then read. |

`--rate` is per connection. The aggregate target rate is `connections x rate`. In a scenario, the phase's `rate_per_conn` plays the same role, and in an action script it paces every individual send.

The first request of a session is due when the session starts, so a paced connection sends its first message at once. A reconnect starts a new schedule.

### 2.3 Request cap

`-n, --requests N` caps the total messages sent across all workers. Right before each send a worker claims one message from a shared counter, and a worker whose claim comes after the `N`th stops and cancels the run; a watcher task also checks the counter every 5 ms. Because every send is claimed first, the run never sends more than `N` messages. A claim followed by a failed write still uses up one of the `N`. In a scenario the counter is shared by all phases, so the cap applies to the whole scenario: once it is reached, later phases are cancelled as soon as they start.

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
- **Handshake latency** starts before the TCP connect and stops when the HTTP upgrade completes. It covers TCP connect, the TLS handshake for `wss://`, and the HTTP upgrade. It is bounded by `--connect-timeout` (default `5s`); a handshake that takes longer is a `timeout` error.
- **Response time** (paced echo only) is how long the reply took from the moment the request was due: RTT plus the time the request waited past its due time because an earlier reply was slow. Lateness of up to one timer tick (2 ms, or 16 ms on Windows) comes from the client's own timer, not from waiting behind a reply, and is left out. This is the coordinated omission correction: a client that waits for each reply sends fewer requests while the server is slow, so RTT alone records one slow sample for a stall that a steady stream of users would each have felt. Response time records the wait every one of those requests saw. When a scenario phase has a think time, the pause after each reply is deliberate, the schedule restarts after it, and response time is not recorded.
- **DNS resolution and TLS setup are not part of handshake latency.** Before the clock starts, wsblast resolves the target host once, also bounded by `--connect-timeout`, and builds one TLS client configuration from the platform's root certificates (or `SSL_CERT_FILE` / `SSL_CERT_DIR`). Every connection of the run reuses both, so the run keeps the addresses it resolved at the start even if DNS changes during the run. A host that does not resolve fails every slot once with `dns_resolution`, without reconnect attempts.
- **Authentication tokens are not part of handshake latency.** With `--auth-token-cmd`, the command runs once before the run starts and its output becomes the authentication header. With `--auth-token-refresh`, the command runs again on a separate thread during the run; a handshake only reads the current header value, before its clock starts. Neither the command nor the refresh is measured, and no message path reads the token.
- **Address selection.** When the host resolves to several addresses, they are tried with IPv6 and IPv4 interleaved, starting with the resolver's first choice. An attempt that has not connected after 250 ms gets the next address started alongside it, and a failed attempt starts the next address at once (RFC 8305, Happy Eyeballs). The address that accepted a connection is tried first by later connections, so a host such as `localhost` that resolves to `::1` first while the server listens only on `127.0.0.1` costs the race only on the first connections, not on every handshake.
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
| `error_rate` | `errors / attempted`, where `errors` is the sum of `error_breakdown` and `attempted = total_messages_sent + (errors - sent_message_errors)`. `sent_message_errors` are the errors that end a message already counted as sent (see [section 6.2](#62-reading-the-error-rate)). 0 when `attempted` is 0. |

`elapsed_secs` has a floor of 0.0001 s to avoid division by zero.

Throughput uses the larger direction so that it is meaningful in every mode: listen mode never sends, and stream mode may send without replies. In echo mode sent and received differ by at most the requests in flight at the end of the run.

The progress bar and the TUI show a live "Msg/s" computed as received messages divided by elapsed time. It is a progress indicator and can differ from the final `throughput_msg_per_sec`.

### 6.3 Time series

`time_series` breaks the run down into intervals. The interval is 1 second for runs of up to 600 seconds; a longer run uses the whole number of seconds that keeps it within 600 points (6 seconds for an hour, 144 seconds for a day). Intervals start when the run's clock starts.

- **Counters** (`messages_sent`, `messages_recv`, `bytes_sent`, `bytes_recv`, `errors`) are the difference between snapshots of the live counters taken at each interval boundary, so the points add up to the run's totals. `active_connections` is the snapshot at the end of the interval. `throughput_msg_per_sec` uses the larger direction, as the run total does.
- **Latency** is the response time of paced echo runs and the RTT otherwise; `latency_basis` says which (`mixed` for a scenario whose phases differ). A sample belongs to the interval in which its reply arrived. A server stall therefore shows up twice: as lower throughput in the interval where requests stopped being answered, and as high latency in the interval where the late replies arrived.
- **Precision.** Each worker keeps a histogram for its current interval at 2 significant figures, so interval percentiles are within 1%, and hands it to the run once per interval. The run-level percentiles in [section 5](#5-histograms-and-percentiles) keep 3 significant figures.
- **Ends of runs.** The last interval is usually shorter. A final interval shorter than a twentieth of the interval length is merged into the one before it. Scenario phases are laid end to end, with each point's `start_secs` counted from the start of the scenario.

The text and Markdown reports summarise the series in at most 10 rows. When a row covers several intervals, it shows their combined throughput and the highest p99 of any one interval: percentiles cannot be combined, so the row's latency is the worst interval, not a p99 over the whole row. The JSON report has every point.

### 6.4 Live export

While a run is in progress, wsblast can publish its live counters and the latency of its time series ([section 6.3](#63-time-series)) to a monitoring system: `--prometheus-listen ADDR` serves them for scraping (`prometheus` feature, on by default) and `--otel-endpoint URL` pushes them to an OpenTelemetry collector over OTLP/HTTP (`otel` feature, opt-in). [docs/export.md](docs/export.md) shows how to set both up. Both read the same values from one place, so a value means the same thing in either system. In a scenario every phase shares the live counters, so the counters keep counting across phases.

Live values describe the run while it happens; the report is computed from the per-worker metrics merged at the end ([section 1](#1-execution-model)) and remains the record of the run. The live counters are relaxed atomics summed over their shards when they are read, so the fields of one read can be slightly out of step with each other, and the final values can differ from the report by the operations in flight when the run ended.

| Prometheus | OpenTelemetry | Type | Definition |
| :--- | :--- | :--- | :--- |
| `wsblast_messages_sent_total` | `wsblast.messages.sent` | counter | Data frames written successfully: `total_messages_sent` so far. |
| `wsblast_messages_received_total` | `wsblast.messages.received` | counter | Data frames counted as received: `total_messages_recv` so far. |
| `wsblast_bytes_sent_total` | `wsblast.bytes.sent` | counter | Payload bytes of the frames sent, as `total_bytes_sent`. |
| `wsblast_bytes_received_total` | `wsblast.bytes.received` | counter | Payload bytes of the frames received, as `total_bytes_recv`. |
| `wsblast_errors_total` | `wsblast.errors` | counter | Errors of every category recorded so far: the sum of `error_breakdown`. |
| `wsblast_connections_established_total` | `wsblast.connections.established` | counter | Slots that completed their first handshake, as `total_connections_established`. |
| `wsblast_connections_failed_total` | `wsblast.connections.failed` | counter | Failed handshake attempts, counting every failed reconnect attempt. This is not `total_connections_failed`, which counts slots that never connected ([section 7](#7-connection-accounting-and-reconnects)): a target that stays down makes this counter grow with every retry. |
| `wsblast_reconnects_total` | `wsblast.reconnects` | counter | Handshakes that re-established a slot after it lost its connection, as `total_reconnects`. |
| `wsblast_active_connections` | `wsblast.connections.active` | gauge | Connections open at the moment of the read. |
| `wsblast_run_elapsed_seconds` | `wsblast.run.elapsed` | gauge | Seconds since the run's clock started ([section 8.1](#81-duration)), 0 before it starts. In a scenario the clock starts with the first phase and keeps running through the pauses between phases. It stops when the run ends, so the final OTLP export carries the run's length. |
| `wsblast_latency_seconds{basis, quantile}` | `wsblast.latency` (attributes `basis`, `quantile`) | gauge | p50, p90 and p99 (`quantile` `0.5`, `0.9`, `0.99`) of the most recently completed interval of the time series, in seconds. |
| `wsblast_latency_samples{basis}` | `wsblast.latency.samples` (attribute `basis`) | gauge | Number of latency samples in that interval. |

The counters start at 0 when wsblast starts and only grow, so `rate()` in Prometheus gives per-second rates. They never reset during a run; a new run is a new process and starts again at 0.

**Latency.** The latency values are those of one interval of the run's time series, with the interval length and the 2-significant-figure precision of [section 6.3](#63-time-series) (1 second for runs of up to 600 seconds). `basis` is `response_time` for paced echo runs and `rtt` otherwise, as `latency_basis` in the report; in a scenario it is the basis of the phase in progress. The interval exported is the last one that has ended, so the value changes once per interval and lags the run by up to one interval:

- Before the first interval of the run (or of a scenario phase) ends, there is no completed interval: Prometheus lists the two latency families without samples and OpenTelemetry records no point.
- An interval with no replies exports `wsblast_latency_samples 0` and no quantiles.
- A connection hands its samples for an interval to the run with its first reply in a later interval, or when the run ends. A read taken just after an interval boundary can therefore miss the samples of connections that have not received a reply since; the time series in the report has every sample.
- When a run or phase ends, the last interval that ended before the run did stays exported. The partial interval at the very end of the run is not exported.

Percentiles cannot be added up, so a monitoring system cannot compute a percentile over a longer window from these values; use the report for whole-run percentiles.

**Prometheus endpoint.** `--prometheus-listen ADDR` binds `ADDR` before the run starts; an address that cannot be bound, for example a port in use, is a configuration error with exit code 1. Port 0 picks a free port, and wsblast prints the address it serves on to stderr. The endpoint answers `GET /metrics` with the text exposition format 0.0.4 and content type `text/plain; version=0.0.4; charset=utf-8`; other paths and methods get 404, a malformed request line 400 and a request head over 8 KiB 431. Every response closes the connection. A scrape reads the live counters without a lock and holds the time series lock only while it reads one interval's percentiles; a worker handing over its interval samples at that moment (about once per interval, [section 6.3](#63-time-series)) waits for it as it would for another worker. The endpoint closes when the run ends, before the report is printed.

**OTLP export.** With `--otel-endpoint URL`, wsblast posts OTLP protobuf to `URL/v1/metrics` (a URL that already ends in `/v1/metrics` is used as given) every `--otel-interval` (default 10 s), and once more when the run ends, so a run shorter than the interval still exports. Counters are cumulative observable counters and the other values observable gauges. Every point carries the attributes `server.address` (the target URL's host) and `wsblast.mode` (the `--mode` value), and the resource carries `service.name` from `--otel-service-name`, else `OTEL_SERVICE_NAME`, else `wsblast`. `OTEL_EXPORTER_OTLP_HEADERS` (or `OTEL_EXPORTER_OTLP_METRICS_HEADERS`) adds request headers and `OTEL_RESOURCE_ATTRIBUTES` adds resource attributes. The endpoint, protocol, timeout and temporality are set by wsblast and their environment variables are ignored; in particular `OTEL_EXPORTER_OTLP_ENDPOINT` does not turn export on, so an environment set up for other programs does not start exporting. Compression is not supported. Exports run on a thread of their own: a slow or unreachable collector never delays a worker. Each export gets at most the export interval or 5 seconds, whichever is shorter, and is not retried, since the next export carries the cumulative values again. The first failed export prints one warning to stderr (after the run with `--tui`) and the run continues; it does not change the report or the exit code. The final export can delay the end of the process by up to that timeout when the collector does not answer.

### 6.1 What counts as an error

`error_breakdown` counts errors per category: `dns_resolution`, `tcp_connect`, `tls_handshake`, `http_upgrade_rejected`, `protocol_error`, `write_error`, `read_error`, `timeout`, `unexpected_close`, `other`.

- Every failed handshake attempt is one error, including each failed reconnect attempt. A target that stays down for the whole run therefore produces many errors per connection.
- In echo mode, each request that gets no reply within `--message-timeout` is one `timeout` error.
- A read or write failure on an established session is one error.
- In graphql-ws mode, `error` and `connection_error` messages are `protocol_error`.
- A server Close frame is not an error in any mode. In stream and listen mode a stream that ends without a Close frame is not recorded either; in echo mode it is `unexpected_close`.

### 6.2 Reading the error rate

The error rate is failed operations divided by attempted operations. An attempted operation is either a sent message or an attempt that failed before a message was sent. Every error is classified when it is recorded:

| Error | Adds an attempt | Examples |
| :--- | :--- | :--- |
| Failure before a message was sent | Yes | A failed handshake or reconnect attempt, a failed write, a read error in stream or listen mode, a graphql-ws `error` message during the subscription. |
| Failure of a message already counted as sent | No | An echo reply that does not arrive within `--message-timeout`, a read error or a stream that ends while the reply is awaited, a graphql-ws `connection_init` that gets no `connection_ack` (timeout, `connection_error`, close or read error). |

So every failed operation is counted exactly once:

- A target that refuses every connection gives `error_rate = 1.0` (no messages, only failed attempts).
- In echo mode a request that times out is one attempt and one failure. If every request times out, the error rate is 1.0.
- A run where 9 of 12 echo requests are answered and the other 3 time out gives `3 / 12 = 0.25`.
- A request still in flight when the run ends counts as attempted but not as failed (see [section 8.2](#82-shutdown-semantics)).

`--max-error-rate` takes a fraction between 0.0 and 1.0 and passes when `error_rate <= threshold`.

## 7. Connection accounting and reconnects

Connection counts are per worker slot, not per handshake:

| Field | Definition |
| :--- | :--- |
| `total_connections_attempted` | Worker slots the run was configured to open: `-c`, or the sum over scenario phases. |
| `total_connections_established` | Slots that completed at least one handshake. |
| `total_connections_failed` | Started slots that never completed a handshake and recorded a connection error. |
| `total_connections_interrupted` | Started slots whose first handshake was still in progress when the run ended. |
| `total_connections_not_started` | Slots the ramp had not reached when the run ended. |
| `total_reconnects` | Extra handshakes that re-established a slot after it lost its connection. |

`total_connections_attempted = total_connections_established + total_connections_failed + total_connections_interrupted + total_connections_not_started`, and `handshake_latency.count = total_connections_established + total_reconnects`.

A slot is not started when the run ends before the ramp reaches it, usually because `--duration` is shorter than `-c / --ramp-rate` (wsblast warns about this at start) or because the run was interrupted. Such a slot never tried to connect, so it is not a failure and has no entry in `error_breakdown`. A local run with `-c 10 --ramp-rate 5 -d 600ms` reports 10 attempted, 3 established, 0 failed and 7 not started. The text and Markdown reports show the not-started count only when it is non-zero.

A slot is interrupted when the run ends while its first handshake is still in progress: it neither succeeded nor failed, so it has no entry in `error_breakdown` and does not count toward `error_rate`. A slot that recorded a connection error before the run ended, for example a `timeout` followed by a reconnect attempt still in progress, counts as failed. Interrupted slots usually mean `--duration` is too short for the target's handshake time or that the target is accepting TCP connections without completing the upgrade; check `--connect-timeout` and the target. The text and Markdown reports show the interrupted count only when it is non-zero, and a run in which every slot was interrupted exits with code 1, like a run in which every slot failed.

### 7.1 Reconnect policy

A worker reconnects when a handshake fails or an established session is lost (server Close, timeout, read or write error), as long as the run is still going.

- Backoff starts at 100 ms and doubles on each consecutive failure (100, 200, 400, 800, 1600, 3200 ms) up to a cap of 5 s.
- A successful handshake resets the backoff to 100 ms. So after a lost session the first retry waits 100 ms.
- Cancellation interrupts the backoff wait.
- Each successful reconnect adds one sample to `handshake_latency` and one to `total_reconnects`. Each failed attempt adds one error but does not change the slot counts.
- Payload sequence numbers (`{{seq}}`) restart at 0 on each new session.

`--no-reconnect` ends the worker at the first lost session or failed handshake. Its slot stays idle for the rest of the run. In a scenario, a `Close` action also ends the worker without reconnecting.

If the handshake request cannot be built from the configuration at all, the slot records one `other` error and is not retried.

With `--auth-token-refresh`, every handshake attempt, including the first connections of a ramp, workers a stage starts and reconnects, sends the token from the most recent successful run of `--auth-token-cmd`. Connections that are already open keep the token they connected with. A failed refresh keeps the previous token, so a server that rejects an expired token reports its failed reconnects as `http_upgrade_rejected`.

## 8. Run duration and shutdown

### 8.1 Duration

`--duration` is the total wall-clock time of the run, including the connection ramp. One timer starts just before the first worker is spawned, and it is the same start point used for `elapsed`, throughput and the progress bar. A run does not last "ramp plus duration".

The run is cancelled by the first of:

- the `--duration` timer,
- the `--requests` cap,
- Ctrl+C or SIGTERM (Ctrl+C only on Windows),
- `q`, `Esc` or Ctrl+C in the TUI.

`elapsed` is read as soon as the runner sees the cancellation, with or without the progress bar: the progress bar refreshes every 100 ms, but its loop also wakes on cancellation. Local runs with `-c 2 -d 250ms` reported 250.7 ms with the progress bar and 251.5 ms with `--no-progress`.

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
| 500 | 500 | 500 |
| 1000 | 1000 | 998 |
| 2000 | 2000 | 1958 |
| 5000 | 4998 | 4970 |

These numbers depend on the machine, the OS and its load. Paced echo keeps its schedule by sending overdue requests as soon as it wakes, so it reaches the configured rate on average even when the timer wakes late; requests are then sent in small bursts. Stream mode skips missed ticks instead. Windows wakes timers at its default timer resolution of about 15.6 ms, so each paced stream connection there sends at most about 64 msg/s, whatever `--rate` asks for, and paced echo sends in bursts of up to 16 ms of requests. On Windows, reach high stream rates with more connections. For a high aggregate rate, prefer more connections at a moderate per-connection rate.

## 9. SLO evaluation and exit codes

SLOs are evaluated once, on the final merged metrics.

| Flag | Check | Passes when |
| :--- | :--- | :--- |
| `--max-p50 D` | `response_time.p50_us` when the run recorded response time, otherwise `message_rtt.p50_us` | `p50 <= D` |
| `--max-p95 D` | the same, at p95 | `p95 <= D` |
| `--max-p99 D` | the same, at p99 | `p99 <= D` |
| `--max-p999 D` | the same, at p99.9 | `p99.9 <= D` |
| `--max-error-rate F` | `error_rate` | `error_rate <= F` |
| `--min-throughput X` | `throughput_msg_per_sec` | `throughput >= X` |

A paced echo run (`--rate`) records response time, so its latency checks are named `Response time p99` and gate on the latency a steady stream of users would see. An unpaced run gates on RTT and its checks are named `Latency p99`. A latency check with no RTT samples (for example every request timed out) fails with the actual value "no RTT samples". It never passes on an empty histogram.

| Exit code | Meaning |
| :--- | :--- |
| 0 | The run completed, every configured SLO passed (or none were set), and at least one connection was established. |
| 1 | Invalid command-line arguments (unknown flag, missing value, conflicting flags), configuration or runtime error (invalid URL, scheme or duration, latency SLO outside echo mode, invalid scenario, unreadable file, report write failure), or every connection slot failed and no SLO failed. |
| 2 | At least one SLO failed. Also used by `--compare --fail-on-regression` when a regression is found. Nothing else exits with 2. |

SLOs are checked before the "all connections failed" condition. A run where every connection fails and an SLO also fails (for example `--max-error-rate 0.5`) exits with 2, not 1. The report is always printed (and written with `-o`) before the process exits with 1 or 2 after a completed run.

## 10. Known limitations

- **Coordinated omission is corrected only for paced echo.** Response time needs a schedule to measure against, so it is recorded only with `--rate` in echo mode. Unpaced echo (`--rate 0`) is a closed loop with no intended send times: its RTT percentiles still record one slow sample per stall and understate tail latency when the server pauses. Scenario action scripts and phases with a think time do not record response time either. To measure tail latency under stalls, set `--rate`.
- **Memory per connection.** Each connection holds two histograms of about 136 KiB each, so about 270 KB per connection before any socket or TLS buffers; a paced echo connection adds a third for response time, and every connection adds about 20 KB for its time series interval. The run keeps one merged interval histogram per point, at most 600, about 12 MB. 10,000 connections need about 2.7 GB for histograms alone. A local echo run with 1,000 connections at `--rate 1` peaked at about 420 MB resident.
- **Reply matching.** Echo replies are matched by `--match` (default `first`, see [section 3.1](#31-how-replies-are-matched-echo-mode)). With `first`, unsolicited server frames are taken as replies.
- **Single generator.** All load comes from one process. wsblast does not detect when the generator itself is the bottleneck (CPU saturation, file descriptor limit, ephemeral ports). It prints a warning when `-c` plus 64 exceeds the open file limit (`ulimit -n`). Watch the generator's CPU when RTT matters.
- **Timer resolution.** Per-connection rates above about 1000 msg/s are approximate (see [section 8.3](#83-timer-resolution)).
- **Live export lags by one interval.** The latency that `--prometheus-listen` and `--otel-endpoint` publish is that of the last completed time-series interval, not of the moment of the read, and the final partial interval is not published (see [section 6.4](#64-live-export)).
- **Payload bytes only.** Byte counts exclude framing, TLS and handshake traffic, so they understate wire bandwidth.
- **Request cap overshoot.** `--requests` can be exceeded by up to about one message per connection.
- **Scenario report header.** In a scenario, the JSON report's `target_url` is the first executed phase's target. `phase_targets` lists the target of every executed phase.
