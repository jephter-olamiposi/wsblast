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
11. [Frame capture](#11-frame-capture)
12. [Distributed runs](#12-distributed-runs)

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

The run lasts the sum of the stage durations, `connections` in the report is the highest target, and `total_connections_attempted` counts every worker the run started, so a profile that ramps down and up again (`10s:100,10s:20,10s:100`) attempts more connections than its peak (180 here). No slot is reported as not started. The time series' `active_connections` shows the profile the run achieved; with `--rate`, its throughput follows the profile too. `--stages` cannot be used with `--scenario`, whose phases set their own connections and ramp. A scenario phase can set `stages` instead: it then runs exactly as described here, over the phase's own clock, with the phase's rate, payload, think time and actions on every connection it opens; its configured connection count is the highest target, its duration the sum of its stages, and its `total_connections_attempted` every worker it started.

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

The progress bar shows a live "Msg/s" computed as received messages divided by elapsed time. It is a progress indicator and can differ from the final `throughput_msg_per_sec`.

The TUI (`--tui`) shows the recent past rather than the run so far. Its p50, p90 and p99 are those of the last completed time-series interval ([section 6.3](#63-time-series)) of the run or scenario phase in progress, read as the live export reads them ([section 6.4](#64-live-export)), with the same lag of up to one interval; the panel is labelled with the basis (response time for paced echo, RTT otherwise), and the p99 sparkline has one point per completed interval. Msg/s, errors and the error rate cover the last interval's length of the counter snapshots the TUI takes at its own refresh rate, 5 times a second. The live error rate is `errors / (messages sent + errors)` over that window: the live counters do not single out errors that end an already-sent message ([section 6.2](#62-reading-the-error-rate)), so it reads lower than `error_rate` when replies time out. Configured SLO thresholds are shown next to the matching live value and colored when it is past the threshold; p95 and p99.9 have no live value, and a minimum throughput is compared with the window, not the whole-run average. This is an indication only: the gate ([section 9](#9-slo-evaluation-and-exit-codes)) is evaluated on the final metrics.

### 6.3 Time series

`time_series` breaks the run down into intervals. The interval is 1 second for runs of up to 600 seconds; a longer run uses the whole number of seconds that keeps it within 600 points (6 seconds for an hour, 144 seconds for a day). Intervals start when the run's clock starts.

- **Counters** (`messages_sent`, `messages_recv`, `bytes_sent`, `bytes_recv`, `errors`) are the difference between snapshots of the live counters taken at each interval boundary, so the points add up to the run's totals. `active_connections` is the snapshot at the end of the interval. `throughput_msg_per_sec` uses the larger direction, as the run total does.
- **Latency** is the response time of paced echo runs and the RTT otherwise; `latency_basis` says which (`mixed` for a scenario whose phases differ). A sample belongs to the interval in which its reply arrived. A server stall therefore shows up twice: as lower throughput in the interval where requests stopped being answered, and as high latency in the interval where the late replies arrived.
- **Precision.** Each worker keeps a histogram for its current interval at 2 significant figures, so interval percentiles are within 1%, and hands it to the run once per interval. The run-level percentiles in [section 5](#5-histograms-and-percentiles) keep 3 significant figures.
- **Ends of runs.** The last interval is usually shorter. A final interval shorter than a twentieth of the interval length is merged into the one before it. Scenario phases are laid end to end, with each point's `start_secs` counted from the start of the scenario.

The text and Markdown reports summarise the series in at most 10 rows. When a row covers several intervals, it shows their combined throughput and the highest p99 of any one interval: percentiles cannot be combined, so the row's latency is the worst interval, not a p99 over the whole row. The JSON report has every point.

The HTML report (`--format html`, or `wsblast render` of a JSON report) plots the series without summarising it: every point is drawn, at the middle of its interval, exactly as defined above, so the worst-interval rule of the text and Markdown tables does not apply. The latency chart shows each interval's p99 of the basis named in `latency_basis`; an interval without replies has no latency and is drawn as a gap, as is the pause between scenario phases. When the p99 values span more than a factor of 50, the latency chart uses a log scale and says so.

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

**Distributed runs.** With `--agents`, the exporters run on the coordinator and export the cluster's values ([section 12.5](#125-scenarios-capture-tokens-and-live-values)): the counters and open connections are the sums of the agents' latest one-second snapshots, so they trail the agents by up to one second more than a single machine's, and the latency is that of the merged interval histograms of every agent, exported one second after the interval ended on the coordinator's clock, so it lags by up to one interval plus one second.

**Prometheus endpoint.** `--prometheus-listen ADDR` binds `ADDR` before the run starts; an address that cannot be bound, for example a port in use, is a configuration error with exit code 1. Port 0 picks a free port, and wsblast prints the address it serves on to stderr. The endpoint answers `GET /metrics` with the text exposition format 0.0.4 and content type `text/plain; version=0.0.4; charset=utf-8`; other paths and methods get 404, a malformed request line 400 and a request head over 8 KiB 431. Every response closes the connection. A scrape reads the live counters without a lock and holds the time series lock only while it reads one interval's percentiles; a worker handing over its interval samples at that moment (about once per interval, [section 6.3](#63-time-series)) waits for it as it would for another worker. The endpoint closes when the run ends, before the report is printed.

**OTLP export.** With `--otel-endpoint URL`, wsblast posts OTLP protobuf to `URL/v1/metrics` (a URL that already ends in `/v1/metrics` is used as given) every `--otel-interval` (default 10 s), and once more when the run ends, so a run shorter than the interval still exports. Counters are cumulative observable counters and the other values observable gauges. Every point carries the attributes `server.address` (the target URL's host) and `wsblast.mode` (the `--mode` value), and the resource carries `service.name` from `--otel-service-name`, else `OTEL_SERVICE_NAME`, else `wsblast`. `OTEL_EXPORTER_OTLP_HEADERS` (or `OTEL_EXPORTER_OTLP_METRICS_HEADERS`) adds request headers and `OTEL_RESOURCE_ATTRIBUTES` adds resource attributes. The endpoint, protocol, timeout and temporality are set by wsblast and their environment variables are ignored; in particular `OTEL_EXPORTER_OTLP_ENDPOINT` does not turn export on, so an environment set up for other programs does not start exporting. Compression is not supported. Exports run on a thread of their own: a slow or unreachable collector never delays a worker. Each export gets at most the export interval or 5 seconds, whichever is shorter, and is not retried, since the next export carries the cumulative values again. The first failed export prints one warning to stderr (after the run with `--tui`) and the run continues; it does not change the report or the exit code. The final export can delay the end of the process by up to that timeout when the collector does not answer.

### 6.5 Time-series metrics in comparisons

`--compare` adds six metrics derived from the time series ([section 6.3](#63-time-series)) when both reports have one. Reports from wsblast 0.2.0 and earlier have no time series, and a comparison with such a report leaves these metrics out, as it does `response_time.*` when one report has no response time. Each metric summarises one report on its own, so two reports with different interval lengths can be compared; points are never paired across reports.

**Window.** `--compare-window START..END` restricts these metrics to the points whose whole interval lies inside the window: `start_secs >= START` and `start_secs + duration_secs <= END`, in seconds since the run started. Either side may be left out (`10s..` skips the warm-up, `..50s` drops the tail) and both take the duration format of `--duration`. START must be before END. Interval boundaries are sampled by a timer and a point usually starts a moment after its nominal boundary, so both edges allow a twentieth of the interval length of slack: with 1-second intervals, `10s..50s` keeps the 40 points that start at 10 s to 49 s. Without the flag every point is used. Whole-run metrics (totals, rates and percentiles) ignore the window. With a window, `--compare` exits with 1 when a report has no time series or no point inside the window, so a gate on these metrics never passes because they were left out.

In the definitions below, *W* is the set of points inside the window. A *full-length* point of *W* is one whose `duration_secs` is at least 95% of `interval_secs`: this leaves out the shorter last interval of a run or of a scenario phase, which covers too little time for a comparable rate. For a full-length point *i*, *r_i* is its `throughput_msg_per_sec` and *d_i* its `duration_secs`.

| Metric | Better | Definition | Undefined (left out) when |
| :--- | :--- | :--- | :--- |
| `series.throughput_mean` | higher | Mean throughput weighted by duration: *m* = Σ *r_i d_i* / Σ *d_i* over the full-length points. | *W* has no full-length point. |
| `series.throughput_cv` | lower | Coefficient of variation of the per-interval throughput: sqrt(Σ *d_i* (*r_i* - *m*)² / Σ *d_i*) / *m*. 0 for perfectly steady throughput; a sawtooth between 50% and 150% of the mean gives 0.5. | Fewer than two full-length points, or *m* = 0. |
| `series.p99_median_us` | lower | Median of `latency.p99_us` over the points of *W* with at least one latency sample (`latency.count > 0`); for an even number of points, the mean of the two middle values. | No point of *W* has a latency sample. |
| `series.p99_worst_us` | lower | Largest `latency.p99_us` of those points: the p99 of the worst interval. | No point of *W* has a latency sample. |
| `series.error_intervals` | lower | Number of points of *W* with `errors > 0`. | *W* is empty. |
| `series.max_interval_errors` | lower | Largest `errors` of any point of *W*. | *W* is empty. |

A metric that is undefined for one report is not compared. The comparison rules for every other metric apply ([docs/ci.md section 7](docs/ci.md#7-baseline-comparison-rules)): a change counts when its relative size is strictly greater than `--compare-threshold`, a metric that is 0 in both reports is skipped, and one that is 0 in the baseline and not in the current report is a 100% change.

How to read them:

- A stall of the server shows up in `series.p99_worst_us` (the interval in which the late replies arrive) and in `series.throughput_cv` (the interval in which replies stopped), even when it changes the run's totals and whole-run p99 by less than the threshold.
- `series.p99_median_us` is the typical interval's p99, which a single bad interval does not move; compare it with `series.p99_worst_us` to tell a general slowdown from an isolated one.
- `series.throughput_cv` and the error metrics are often 0 or close to 0 in a healthy run, so a small absolute change is a large relative one: a baseline CV of 0.01 against 0.02 is a 100% regression. Gate on them only when the baseline is clearly above 0, or choose the metrics a gate fails on as shown in [docs/ci.md section 4.3](docs/ci.md#43-baseline-comparison-against-the-last-run-on-main).
- Interval percentiles have 1% precision ([section 6.3](#63-time-series)), so changes of `series.p99_*` below about 2% are within rounding.

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
| `total_connections_attempted` | Worker slots the run was configured to open: `-c`, or the sum over scenario phases. A staged run or phase counts every worker it started ([section 2.1.1](#211-load-stages)). |
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

SLOs are evaluated once, on the final merged metrics. A scenario adds per-phase SLOs, evaluated on each phase's own metrics ([section 9.1](#91-scenario-phases)).

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
| 1 | Invalid command-line arguments (unknown flag, missing value, conflicting flags), configuration or runtime error (invalid URL, scheme or duration, latency SLO outside echo mode, invalid scenario, unreadable file, report write failure), every connection slot failed and no SLO failed, or, in a distributed run, an agent failed before the start or was lost after it and no SLO failed ([section 12.4](#124-lost-agents)). |
| 2 | At least one SLO failed. Also used by `--compare --fail-on-regression` when a regression is found. Nothing else exits with 2. |

### 9.1 Scenario phases

Each scenario phase runs its own workers, so its metrics are built the way a single run's are: when the phase ends, the histograms and counters of that phase's workers are added up into the phase's totals and summarised with the phase's own elapsed time and configured connection count. Throughput is the phase's messages over the phase's elapsed time, the error rate the phase's failed operations over its attempted operations, and the percentiles those of the phase's histograms. The phase keeps its own time series, counted from the phase start. The phase totals are then added to the scenario totals, and the merged metrics are summarised from those, so they are exactly what merging every worker of every phase at once would give: counters and histogram samples of the phases add up to the merged values, and `elapsed` is the sum of the phases' elapsed times.

A phase's `slo` mapping is checked on the phase's metrics with the same rules as the flags above: its latency thresholds gate on response time when the phase recorded it (paced echo without actions or think time) and on RTT otherwise, and a latency threshold without samples fails. The scenario's top-level `slo` mapping and the SLO flags are checked together on the merged metrics; a flag replaces the top-level threshold of the same name, and flags never change a phase's thresholds. The report lists the checks of the merged metrics first and then each phase's, prefixed with the phase name (`spike: Latency p99`). The run exits with 2 when any of them fails. See [docs/scenarios.md](docs/scenarios.md), section 7.

SLOs are checked before the "all connections failed" condition. A run where every connection fails and an SLO also fails (for example `--max-error-rate 0.5`) exits with 2, not 1. The report is always printed (and written with `-o`) before the process exits with 1 or 2 after a completed run.

## 10. Known limitations

- **Coordinated omission is corrected only for paced echo.** Response time needs a schedule to measure against, so it is recorded only with `--rate` in echo mode. Unpaced echo (`--rate 0`) is a closed loop with no intended send times: its RTT percentiles still record one slow sample per stall and understate tail latency when the server pauses. Scenario action scripts and phases with a think time do not record response time either. To measure tail latency under stalls, set `--rate`.
- **Memory per connection.** Each connection holds two histograms of about 136 KiB each, so about 270 KB per connection before any socket or TLS buffers; a paced echo connection adds a third for response time, and every connection adds about 20 KB for its time series interval. The run keeps one merged interval histogram per point, at most 600, about 12 MB. 10,000 connections need about 2.7 GB for histograms alone. A local echo run with 1,000 connections at `--rate 1` peaked at about 420 MB resident.
- **Reply matching.** Echo replies are matched by `--match` (default `first`, see [section 3.1](#31-how-replies-are-matched-echo-mode)). With `first`, unsolicited server frames are taken as replies.
- **Single generator.** Without `--agents`, all load comes from one process ([section 12](#12-distributed-runs) spreads it over several machines). wsblast does not detect when the generator itself is the bottleneck (CPU saturation, file descriptor limit, ephemeral ports). On Unix it prints a warning when the connection count (`-c`, or the peak of `--stages`) plus 64 exceeds the soft open file limit (`ulimit -n`). Watch the generator's CPU when RTT matters.
- **Timer resolution.** Per-connection rates above about 1000 msg/s are approximate (see [section 8.3](#83-timer-resolution)).
- **Live export lags by one interval.** The latency that `--prometheus-listen` and `--otel-endpoint` publish is that of the last completed time-series interval, not of the moment of the read, and the final partial interval is not published (see [section 6.4](#64-live-export)).
- **Payload bytes only.** Byte counts exclude framing, TLS and handshake traffic, so they understate wire bandwidth.
- **Request cap counts claimed sends.** `-n` is exact: the run never sends more than N messages ([section 2.3](#23-request-cap)), and a distributed run splits N exactly between its agents ([section 12.1](#121-what-each-agent-runs)). A send that was claimed but failed, or was abandoned because the run stopped, still uses up one of the N, so `total_messages_sent` can end slightly below N. Reaching the cap stops the run as the timer does ([section 8.2](#82-shutdown-semantics)), so in echo mode up to one request per connection is counted as sent and not received.
- **Scenario report header.** In a scenario, the JSON report's `target_url` is the first executed phase's target. `phase_targets` lists the target of every executed phase, and `phases` the metrics and SLO result of each.
- **Phase percentiles do not combine.** The merged percentiles come from one histogram over every phase's samples, so they are not an average of the phases' percentiles, and a short slow phase can pass the merged p99 while failing its own. Gate such a phase with its own `slo`.

## 11. Frame capture

`--capture FILE` (feature `capture`) writes a JSON Lines record of the run. The first line is a header:

```json
{"wsblast_capture":1,"target":"ws://127.0.0.1:9001/","started":"2026-10-25T09:30:00.123Z"}
```

`wsblast_capture` is the format version, `target` the parsed target URL (the first phase's for a scenario) and `started` the wall-clock start in RFC 3339 UTC. Every following line is one record:

```json
{"t_us":1532,"worker":0,"dir":"recv","kind":"open","len":0}
{"t_us":1611,"worker":0,"dir":"sent","kind":"text","len":42,"data":"{\"source\":\"wsblast\",\"ts\":\"1761384600123\",\"seq\":0}"}
{"t_us":1702,"worker":0,"dir":"recv","kind":"binary","len":3,"data":"AP8B"}
{"t_us":2001544,"worker":0,"dir":"sent","kind":"close","len":0}
```

| Field | Meaning |
| :--- | :--- |
| `t_us` | Microseconds from the start of the run (the clock of [section 8.1](#81-duration); for a scenario, the scenario start) to the moment the record was taken. |
| `worker` | The connection slot, from 0. A slot keeps its number across reconnects. Every scenario phase numbers its slots from 0 again, and all phases write to the same file. |
| `dir` | `sent` or `recv`, from the client's side. `open` is always `recv`. |
| `kind` | `text`, `binary`, `close` or `open`. |
| `len` | Payload bytes: the text's UTF-8 length, the binary length, 2 plus the reason's length for a Close frame with a code, and 0 for a code-less Close frame and for `open`. |
| `code` | Close frames with a code only: the close code. |
| `data` | The text as is, the binary payload as padded standard base64 (RFC 4648), or a Close frame's reason. Absent for `open` and code-less Close frames. |

What is recorded:

- An `open` record when a handshake completes, first connection or reconnect.
- Every text, binary and Close frame the session loop hands to the socket (`sent`, taken when the socket accepts the frame, before it is flushed) or reads from it (`recv`, taken when the frame is read). That includes frames the report does not count: unsolicited frames skipped by `--match`, GraphQL protocol messages and the Close frames of the shutdown in [section 8.2](#82-shutdown-semantics). A frame whose write is accepted but then fails, and a reply read in the same instant the run is cancelled, can therefore appear in the capture but not in the report's counters.
- Not recorded: pings, pongs, the Close reply the WebSocket library sends on its own, and a connection that ends without a Close frame.

On a clean echo run, the `sent`/`text` (or `binary`) records equal `total_messages_sent` and the `recv` ones equal `total_messages_recv`.

### 11.1 Cost and drops

Workers do not write the file. A session with capture on wraps its socket in an adapter that, for each frame, reads the clock, takes a reference to the frame's payload (the payload bytes are not copied) and offers the record to a bounded queue of 65,536 records with a non-blocking `try_send`. One writer thread drains the queue, formats the JSON (base64 for binary frames happens there, not in the worker), and owns a 256 KiB buffered file. The writer polls the queue every 2 ms when it finds it empty instead of sleeping until a record arrives, so a worker never has to wake it with a system call. A run without `--capture` creates no queue and no thread, and its sessions run on the bare socket.

Measured with release builds on a 2-core Linux VM, against `examples/echo_server.rs` on the same machine, 4 connections for 3 s, three runs each:

| Run | Without capture | With `--capture --capture-limit 0` |
| :--- | :--- | :--- |
| Unpaced echo, throughput | 54,400 to 57,700 msg/s | 59,000 to 63,200 msg/s, 354,000 to 380,000 records written, 0 dropped |
| Unpaced echo, RTT p50 / p99 | 54 to 60 µs / 222 to 387 µs | 48 to 53 µs / 230 to 278 µs |
| `--rate 1000`, RTT p50 | 98 to 116 µs | 100 to 105 µs |

The differences are within the run-to-run spread. A first version whose writer blocked on the queue cost about a third of the unpaced throughput and raised the paced RTT p50 by about 60 µs, from the wake-ups alone.

A record is dropped, and counted, when:

- `--capture-limit` records (default 100,000; 0 means unlimited) have already been queued: the limit is exact, so the file holds at most that many records after the header;
- the queue is full because the writer is behind the workers;
- or a write to the file failed: the error is printed on stderr and every later record is counted as dropped.

The workers never wait, so capture cannot slow the load down by more than the per-frame cost above, but a run that produces frames faster than the disk takes them loses records rather than slowing down. `frames_written + frames_dropped` is every record the run produced. The writer is flushed and the file closed after every worker has stopped and before the report is printed. The report prints `Capture: N frames written, M dropped (FILE)`, and the JSON report carries `metrics.capture` with `path`, `frames_written`, `frames_dropped` and `limit` (absent when the run did not capture).

With `--agents`, every agent records its frames with the same queue, limit check and drop counting, and sends them to the coordinator, which writes one file with an `agent` field on every record ([section 12.5](#125-scenarios-capture-tokens-and-live-values)).

`wsblast mock --replay FILE` sends the `recv` frames of a capture back to new connections with their recorded timing; see [docs/mock.md](docs/mock.md). A capture is meant for debugging and replay, not for high-rate runs: keep the default limit, or expect drops, when a run sends more than a few hundred thousand frames per second.

## 12. Distributed runs

With `--agents` (feature `distributed`), a coordinator splits one run between several machines running `wsblast agent`, starts them together and merges their results. The coordinator generates no load. [docs/distributed.md](docs/distributed.md) describes the setup, the protocol and the security model; this section defines the numbers.

### 12.1 What each agent runs

Each agent runs its share of the configuration as an ordinary run on its own machine: the same workers, pacing, reconnects, timers and shutdown as sections 1 to 8, and its own live counters, request cap and time series. The share:

- `-c`, `-n` and `--ramp-rate` are divided as evenly as possible, the remainder going to the first agents in `--agents` order, one each (`-c 10` over 3 agents: 4, 3, 3).
- `--stages`: every agent follows the whole profile and opens its part of the target at every moment, `target(t)` divided as above, so the agents' counts add up to the profile's target at every point ([section 2.1.1](#211-load-stages)).
- `--rate` is per connection and unchanged; every other setting is unchanged.
- A share of zero (more agents than connections, than `-n` or than a non-zero `--ramp-rate`) is rejected before anything connects.

Before the start, each agent rebuilds its configuration with the same validation as the command line, resolves the target and builds its TLS settings, as a single run does before its clock starts ([section 4](#4-timing)), and reports ready. The coordinator starts the agents only when every one of them is ready.

### 12.2 How results are merged

Every agent sends, once, its counters, its error breakdown, the number of slots that were interrupted and not started, and its three histograms (handshake, RTT, response time) as the `(value, count)` pair of every non-empty bucket. Rebuilt with the same bounds and precision ([section 5.1](#51-bounds-and-precision)), each histogram has exactly the agent's buckets, and the coordinator adds the rebuilt histograms as a single run adds its workers' histograms. So:

- Counters, byte counts, error categories, reconnects, unsolicited frames and slot counts are sums over the agents.
- Percentiles, minimum, maximum, mean and standard deviation are computed on the merged histograms: the p99 of a distributed run is the p99 of every sample from every agent, identical to the p99 one process with all the workers would compute from the same samples. Percentiles are never averaged.
- `total_connections_attempted` is the sum of the agents' slots, so the identities of [section 7](#7-connection-accounting-and-reconnects) hold for the merged report, and `error_rate` uses the merged counts ([section 6](#6-counters-rates-and-formulas)).
- The live counters each agent sends every second drive the coordinator's progress bar and live export ([section 12.5](#125-scenarios-capture-tokens-and-live-values)). They are never merged into the report.

### 12.3 Time across machines

No measurement uses a wall clock or compares clocks of different machines. Every latency sample is taken on one agent with its monotonic clock, so the agents' clocks need no synchronisation.

- **Start skew.** The coordinator sends `start` to every agent at once, on concurrent writes, as soon as the last one is ready. Agent `i` starts when the message arrives, one control-channel latency `d_i` after it was sent. The start times of two agents therefore differ by at most `max(d_i) - min(d_i)`, which is below the largest one-way latency and so below the largest control round trip. The coordinator measures that round trip during the hello exchange and prints it as the start skew bound. On one network segment it is well under a millisecond; between regions it can be tens of milliseconds.
- **`elapsed`** is the coordinator's time from sending `start` to receiving the last result, with no grace period added or subtracted. It is longer than any agent's own run time by the latency of the start message plus the time to encode and transfer the last result, usually milliseconds, and throughput (`max(sent, received) / elapsed`) is that much lower than the sum of the agents' own throughputs. A lost agent ([section 12.4](#124-lost-agents)) does not extend `elapsed`: the clock stops at the last result that arrived.
- **Time series.** Each agent's series is counted from its own start, and points with the same interval index are merged: counters and open connections are summed, the duration is the longest of the agents' for that index, throughput is recomputed from the sums, and the latency comes from the sum of the agents' interval histograms, which they send next to their series. So interval percentiles are also those of every sample. Interval `k` of the merged series covers each agent's `[k, k + 1)` intervals, which are shifted against each other by the start skew. `active_connections` adds values sampled up to one skew apart.
- **Duration and limits.** Each agent ends its own run at its `--duration` or its share of `-n`. Ctrl+C or SIGTERM on the coordinator sends `stop`, and each agent ends its run as on a local Ctrl+C ([section 8.2](#82-shutdown-semantics)) and still sends its result.

### 12.4 Lost agents

An agent is lost when, after the start, it disconnects, reports an error, or sends no result within `--agent-timeout` (default 30 s) after the end of its duration, or after a `stop`. A lost agent's measurements never reach the coordinator, and its work is not given to another agent, so nothing is counted twice: the report covers the agents that finished, as if the lost agents' connections had never been configured. The text and Markdown reports show `Agents: N (lost: ...)`, the JSON report lists every agent in `metrics.distributed.agents` and the lost ones in `metrics.distributed.lost_agents`, and the coordinator prints a warning naming each lost agent. SLOs are evaluated on the merged metrics as usual; the run exits with 2 if one fails and otherwise with 1, because part of the configured load is missing. A failure before the start (an unreachable agent, a rejected token or configuration, a busy agent, a protocol mismatch, or no ready reply within `--agent-timeout`) exits with 1 and nothing runs.

### 12.5 Scenarios, capture, tokens and live values

**Scenario phases.** With `--scenario`, each phase is split, started behind its own barrier and merged as a single distributed run is (sections 12.1 to 12.3), and its metrics are those of [section 9.1](#91-scenario-phases) computed on the merged histograms and counters: the phase's `elapsed` is the coordinator's time from the phase's `start` to its last result, its counters and histograms are the sums over the agents, and its SLOs are checked on them. The scenario's merged metrics add up the phases' merged totals, `elapsed` is the sum of the phases' elapsed times, the scenario's SLOs are checked on them, and the phases' merged time series are laid end to end at the coordinator's phase start times. An agent lost in a phase (section 12.4) takes no part in the later phases and its shares of them are not given to the other agents, so the identities of section 7 still hold for every phase and for the run.

**Capture.** Each agent applies [section 11.1](#111-cost-and-drops) unchanged: the bounded queue, the non-blocking enqueue and the drop counting are the same, and the thread that drains the queue sends the records to the coordinator instead of writing them, waiting when the control channel is behind, so a slow channel or coordinator disk drops records on the agent instead of slowing its workers. The coordinator shifts each record's `t_us` by the time between the start of the capture and its own start of the record's phase, so records of different agents are aligned to within the start skew of section 12.3. `--capture-limit` is the limit of the whole run: the coordinator writes records up to it in arrival order and counts the rest as dropped, and each agent may queue at most what was left of it when the phase started. `frames_dropped` is the sum of the agents' drops, the records over the limit and the records lost to a write error; `frames_written + frames_dropped` is every record the finished agents produced.

**Refreshed tokens.** With `--auth-token-refresh`, the token command runs on the coordinator; every new header value reaches each agent in a control message, checked every 100 ms, and applies to that agent's handshakes from then on, as a refresh does on one machine. A handshake on an agent can therefore carry the previous value for up to about 100 ms plus one control-channel latency after the refresh.

**Live values.** The coordinator keeps the cluster's live counters as the sum of each agent's latest snapshot, sent every second during a phase and once more at its end. With an exporter ([section 6.4](#64-live-export)), the exported latency is read from the sum of the agents' interval histograms for the last interval that ended at least one second ago on the coordinator's clock: each snapshot carries the agent's two most recently completed intervals, so that interval has been sent by every agent at its next tick, including the samples workers handed over after the interval ended. When a phase ends, the last interval that ended before it is exported from the agents' results. These values are never used for the report.
