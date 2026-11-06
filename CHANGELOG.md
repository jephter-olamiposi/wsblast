# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- docs/benchmarks.md compares wsblast with k6 and Artillery on throughput per core, memory per connection and a stalling server, and `scripts/compare/run.sh` reproduces it.

### Fixed
- `install.sh` no longer fails when the GitHub API is rate limited or blocked: it finds the latest release through the release page instead.

## [0.2.2] - 2026-11-06

0.2.2 cuts memory per connection from about 575 KB to about 16 KB, so 4,000 connections need about 140 MB instead of 2.3 GB, with identical percentiles. It adds scripted mock replies (`wsblast mock --rules`) and a slow-consumer fault (`--chaos-read-pause`), records the rate and load stages in JSON reports, and changes `-o` to keep the terminal readable. The library API is a compatible superset of 0.2.1.

### Changed
- **Memory per connection is about 36 times lower.** Each connection used to hold its own latency histograms (about 136 KiB each for handshake latency, round-trip time and response time), a 20 KB time-series histogram and the WebSocket library's 128 KiB read buffer: about 575 KB per connection, so 4,000 connections needed 2.3 GB. Every worker of a run now records into one shared set of histograms with lock-free atomic counters, split into one shard per CPU core, and moves its time-series samples to the run in batches of 64. Read buffers start at an equal share of 64 MiB, between 4 KiB and 128 KiB, so runs of up to 512 connections keep the 128 KiB default. Each additional connection now costs about 16 KB, and 4,000 connections use about 140 MB. Percentiles are unchanged: the shared histograms use the same buckets, so the merged histogram is identical. See METHODOLOGY.md, section 10.
- With `-o FILE`, the report is written to the file in the `--format` format and the terminal shows the text summary. Before, the report was also printed to stdout in the file's format, so a JSON or HTML report filled the terminal or CI log. Without `-o`, the report is printed to stdout as before. `--compare` follows the same rule.

### Added
- `--chaos-read-pause DURATION[:JITTER]` (scenario key `read_pause`): a slow consumer. After every received data frame the client waits before reading the next, so the server's frames back up in the socket buffers while sends and keepalives go on. It works in every mode, `listen` and `graphql-ws` included, where the other chaos options do not apply. The pause is never latency: in echo mode it follows the recorded reply, and in paced echo it moves the schedule like `--chaos-delay`. Reports count it as `read_pauses`, and it draws from a generator of its own, so the send decisions of a seeded run stay the same. See METHODOLOGY.md, section 13.
- `wsblast mock --rules FILE` answers frames from a YAML rules file instead of echoing. A rule matches a text frame, or a JSON value at a pointer (optionally equal to a value), from the connection's N-th frame on (`after`), and replies with templated text frames (`{{json:/pointer}}`, `{{seq}}`, `{{request}}`), after its own delay, then optionally closes with a code and reason. The first matching rule wins; unmatched frames are echoed or ignored. Templates that copy the request's id pair with the client's `--match json:/path`, so a load test can time each request to its own reply while the mock pushes other frames. The library equivalent is `MockConfigBuilder::rules`. See docs/mock.md, section 9.
- JSON reports record `rate_per_conn` (`--rate`) and `stages` (`--stages`), both optional in `schema/report-v1.json`, so `wsblast render` shows the rate and the load stages of a saved run as the live report does. Scenario reports leave out `rate_per_conn`, since each phase sets its own rate.
- CI runs a performance budget on every push: a release build must stay under 12 MiB, start in under 100 ms, use at most 48 KB per additional connection and echo at least 5,000 messages per second over 50 connections. The script is `scripts/perf-budget.sh`, and it writes its measurements to the job summary.

### Fixed
- Output piped into a reader that exits early (`wsblast ... | head -n 5`) no longer makes wsblast panic. The rest of the output is dropped, the run carries on, and the exit code still reports the SLO result.

## [0.2.1] - 2026-11-05

0.2.1 brings back, rebuilt and tested, the features 0.2.0 removed: Prometheus and OpenTelemetry export, frame capture, the mock server, distributed load generation, chaos injection, and JWT and OAuth2 authentication. Only `compression` stays out, because the WebSocket library has no permessage-deflate support. It also corrects coordinated omission in paced runs, records a per-interval time series in every report, adds load stages, HTML reports and per-phase scenario SLOs, and rebuilds the live dashboard. The library API is a compatible superset of 0.2.0.

### Migrating from 0.2.0

- **SLO gates on paced runs.** With `--rate`, `--max-p50`, `--max-p95`, `--max-p99` and `--max-p999` now gate on response time from the send schedule, not round-trip time. A server that cannot keep up with `--rate` can now fail a gate it passed on 0.2.0. Raise the threshold, lower `--rate`, or drop `--rate` to keep gating on round-trip time.
- **`-n, --requests` is exact.** A run sends at most the cap. 0.2.0 could overshoot by about one message per connection, so totals in a gate on `-n` runs may drop slightly.
- **New report fields.** The JSON report adds `response_time`, `time_series`, `total_connections_interrupted`, `phases`, `capture`, `distributed` and `generator_lag`. Its schema version stays `1.0.0`, every new field is optional, and `wsblast render` and `--compare` still read 0.2.0 reports. A comparison of two 0.2.0 reports prints exactly what 0.2.0 printed.
- **New default features.** `prometheus`, `capture`, `mock`, `distributed` and `chaos` are on by default; `otel`, `jwt` and `oauth2` are opt-in. Library users who want a smaller dependency set can use `default-features = false` and enable only what they need.
- **DNS is resolved once.** The target is resolved before the run starts, so handshake latency no longer includes DNS, and a DNS change during a run is not picked up.

### Added
- Coordinated omission correction for paced echo runs. With `--rate`, wsblast records a new `response_time` distribution: the time from when each request was due on the connection's schedule to its reply. When the server is slower than the configured rate, requests queue behind the slow replies, and response time shows that wait while round-trip time does not. The text and Markdown reports add a "Response Time (from schedule)" column, the JSON report a `response_time` object, and `--compare` compares its percentiles when both reports have them. See METHODOLOGY.md section 4.
- Per-interval time series. Every report now breaks the run down by time: messages, bytes, errors, active connections, throughput and latency percentiles per interval. The interval is 1 second for runs up to 10 minutes and longer for longer runs, so a series never exceeds 600 points. The JSON report has a `time_series` object with every point; the text and Markdown reports summarise it in at most 10 rows. The library exports `TimeSeries`, `TimeSeriesPoint` and `IntervalLatency`. See METHODOLOGY.md section 6.3.
- Load stages. `--stages 30s:100,2m:100,30s:0` (and `LoadTestConfigBuilder::stages` with the new `Stage` type) runs a load profile instead of a constant connection count: each stage moves the number of open connections linearly to its target, and ramping down closes the most recently opened connections first. It replaces `-c`, `-d` and `--ramp-rate`, which it cannot be combined with, and the report lists the stages. See METHODOLOGY.md section 2.1.1.
- Distributed load generation (`distributed` feature, on by default). `wsblast agent --listen ADDR` runs a load agent that needs a token (`--token` or `WSBLAST_AGENT_TOKEN`, at least 16 characters) and can serve its control channel over TLS (`--tls-cert`, `--tls-key`). `wsblast URL --agents HOST:PORT,...` splits the run between the agents (connections, `-n` and `--ramp-rate` divided as evenly as possible, `--stages` shared at every moment), starts them together once every agent is ready (`--agent-timeout`, default 30s), and merges their results: counters are added and the latency histograms themselves are merged, so percentiles are exact. The coordinator generates no load. Ctrl+C stops every agent and still reports. A lost agent is named in a warning and in the report (`Agents: N (lost: ...)`, `metrics.distributed` in JSON) and the run exits with 1 unless an SLO failed. Scenarios, capture, token refresh, JWT and OAuth2, live export, chaos, the generator-lag gate and the TUI all work with `--agents` (see below). The library exposes the agent and the coordinator as `wsblast::distributed`. See docs/distributed.md and METHODOLOGY.md section 12.
- HTML report. `--format html` writes one self-contained HTML file: inline CSS and SVG, no JavaScript and nothing loaded from the network, so it opens offline and as a CI artifact, in light or dark mode. It shows the run's settings, a pass or fail banner with every SLO check, throughput, error rate and the gating latency's p50 and p99, the latency percentiles, the error breakdown, and charts of throughput, p99 latency, active connections and errors per interval drawn from every time series point. Each chart has a text description and a data table. `render_report` returns it for `OutputFormat::Html`, a new variant. `--compare` does not support it. See docs/ci.md section 8.
- `wsblast render REPORT.json [--format html|markdown|text] [-o FILE]` renders a saved JSON report (schema 1.x, including 0.2.0 reports without a time series or response time) without running a test. HTML is the default. It exits 1 for a missing file or a file that is not a wsblast report, and 0 otherwise whatever the SLO result.
- Scenario files accept an `slo:` mapping at the top level and on each phase (`max_p50`, `max_p95`, `max_p99`, `max_p999`, `max_error_rate`, `min_throughput`). A phase SLO is checked on that phase's own metrics, gating on response time for paced echo phases and on RTT otherwise. The top-level `slo:` and the SLO flags are checked on the merged metrics, and a flag overrides the file's threshold of the same name. Any failing check exits with code 2, and phase checks are named after their phase (`spike: Latency p99`).
- Scenario phases can use `stages:` instead of `connections`, `duration` and `ramp_rate`, either as a list of `{duration, target}` or in the `--stages` syntax (`"30s:100,1m:100,30s:0"`). The phase then runs exactly like `--stages`.
- Scenario reports include per-phase results. The JSON report has a `phases` array with each phase's metrics, stages and SLO result, documented in `schema/report-v1.json` and read back by `wsblast render`. The HTML report has a Phases section. The text and Markdown reports add a phase table when a phase sets an SLO or stages. The library's `ScenarioOutcome` gains `phases` (new `PhaseResult` type), `slo` and `passed()`.
- Time-series metrics in `--compare`. When both reports have a time series, the comparison adds `series.throughput_mean` (higher is better), `series.throughput_cv`, `series.p99_median_us`, `series.p99_worst_us`, `series.error_intervals` and `series.max_interval_errors` (lower is better). They catch a stall, a throughput sawtooth or an error burst that the whole-run totals average away, and `--fail-on-regression` gates on them with the usual threshold. Comparisons with 0.2.0 reports, which have no time series, leave them out and print exactly what 0.2.0 printed. See METHODOLOGY.md section 6.5.
- `--compare-window START..END` (for example `10s..50s`, `10s..` or `..50s`) computes the time-series metrics only over the intervals inside the window, to compare the steady state without warm-up or ramp-down. Whole-run metrics are unaffected. A malformed window, or a report with no time series or no interval inside the window, exits with 1.
- The comparison JSON is schema 1.2.0 (`schema/compare-v1.2.json`) when the time-series metrics are compared or a window is set, and adds an optional `window` object. Other comparisons are still written as 1.1.0, unchanged.
- Paced echo action scripts (`actions` with `rate_per_conn`) follow the fixed send schedule of `--rate` and record response time; `Wait`, `Ping` and think time restart the schedule, and such a phase gates on response time.
- Live export. `--prometheus-listen ADDR` serves `GET /metrics` in the Prometheus text format for the duration of a run (default `prometheus` feature, no new dependencies): message, byte, error, connection and reconnect counters, open connections, elapsed time, and p50/p90/p99 of the last completed time-series interval. `--otel-endpoint URL`, `--otel-interval` (default `10s`) and `--otel-service-name` push the same metrics to an OpenTelemetry collector over OTLP/HTTP (opt-in `otel` feature, rustls only). Export failures never affect the run. See docs/export.md and METHODOLOGY.md section 6.4.
- Frame capture. `--capture FILE` writes a JSON Lines record of every text, binary or Close frame and connection open, with `--capture-limit` (default 100000). Workers never wait for the disk: records past the limit, or produced while the writer is behind, are dropped and counted, and the report shows `Capture: N frames written, M dropped`. See METHODOLOGY.md section 11.
- Mock server. `wsblast mock` runs a local test target that echoes by default and can add `--delay` and `--jitter`, `--close-after N`, `--reject-ratio F` (HTTP 503 on upgrade), `--stall-every`/`--stall-for` stalls, and `--replay FILE` of a `--capture` recording. `--tls-cert`/`--tls-key` serve `wss://`. Also available in-process as `wsblast::mock`. See docs/mock.md.
- Chaos injection (`chaos` feature, on by default): `--chaos-delay DURATION[:JITTER]`, `--chaos-drop PERCENT`, `--chaos-corrupt PERCENT`, `--chaos-close PERCENT` with `--chaos-close-mode abrupt|graceful`, and `--chaos-seed N`, plus a per-phase `chaos:` mapping in scenarios. Injected events are counted in `metrics.chaos` and never as errors or latency samples. See METHODOLOGY.md section 13.
- Load generator saturation detection. A 10 ms runtime-lag probe reports `metrics.generator_lag` (run and per interval), warns when the latency numbers include client-side delay, and gates with `--max-generator-lag` (or `max_generator_lag` in a scenario). The open file limit check is reported as `metrics.open_file_limit`. See METHODOLOGY.md section 14.
- Token authentication from a command. `--auth-token-cmd COMMAND` reads the token for `--auth-type static`, or the key for `--auth-type apikey`, from a command's trimmed stdout before the run, bounded by `--auth-token-cmd-timeout` (default `30s`). `--auth-token-refresh DURATION` re-runs it during the run; new handshakes and reconnects use the latest token. Errors show the exit status and first stderr line, never the token, and authentication values are redacted in Debug output.
- `--auth-type jwt` (opt-in `jwt` feature): wsblast signs its own JWTs (HS256, HS384, HS512, RS256, ES256) with ring. The key comes from `--auth-jwt-key FILE` or `WSBLAST_JWT_KEY_FILE`, never the command line; `--auth-jwt-claims`, `--auth-jwt-ttl` (default `5m`), `--auth-jwt-kid` and `--auth-jwt-per-connection` (a fresh token with a unique `jti` per connection attempt) shape the tokens, and the shared token is re-signed before it expires.
- `--auth-type oauth2` (opt-in `oauth2` feature): an OAuth 2.0 client-credentials token from `--auth-oauth2-token-url`, `--auth-oauth2-client-id`, the secret from `--auth-oauth2-client-secret-file` or `WSBLAST_OAUTH2_CLIENT_SECRET`, and optional `--auth-oauth2-scope` and `--auth-oauth2-audience`, refreshed before `expires_in`. A failed request before the run exits with 1 and shows the HTTP status and `error` code; a failed refresh keeps the previous token and warns.
- The live dashboard (`--tui`) is rebuilt: p50/p90/p99 of the last completed interval (response time or RTT) with a p99 sparkline, throughput, errors and an approximate error rate, reconnects and failed handshakes, the current stage with target and open connections, and the configured SLOs next to their live values (an indication: the final report still gates). It works with `--scenario` (phase in progress, its SLOs, and every finished phase's result) and switches to a compact view below 72x22.
- Distributed runs support everything a single run does except `--auth-jwt-per-connection` (the signing key stays on the coordinator): scenarios phase by phase, `--capture` into one file with an `agent` field per record, `--auth-token-refresh`, JWT and OAuth2 (tokens signed or fetched on the coordinator and pushed to the agents), `--prometheus-listen` and `--otel-endpoint` with cluster totals, chaos with a seed per agent, `--max-generator-lag` gating on the worst agent, and `--tui` on the coordinator. See docs/distributed.md.
- `--compare --format html` writes a self-contained comparison page with a verdict, every compared metric and, when both reports have a time series, baseline and current overlay charts. `wsblast render` also reads comparison JSON (schema 1.1 and 1.2).

### Fixed
- Handshake latency no longer includes DNS resolution or loading the platform's root certificates. The target is resolved and the TLS client configuration is built once, before the run starts, and every connection reuses them. `--connect-timeout` also bounds the initial resolution. Because the addresses are resolved once, DNS changes during a run are not picked up.
- A host that resolves to several addresses no longer stalls each handshake on an unreachable one. Connections race the addresses with IPv6 and IPv4 interleaved (RFC 8305), and later connections try the address that answered first. On Windows, testing `localhost` against a server that listens only on IPv4 previously added about two seconds to every handshake.
- A host that does not resolve fails each connection slot once with `dns_resolution` instead of retrying the lookup for the whole run.
- Connection slots whose first handshake was still in progress when the run ended were counted as failed with no error recorded, next to a 0% error rate. They are now reported in a new `total_connections_interrupted` field, shown in the text and Markdown reports when non-zero. A run in which every slot was interrupted exits with code 1 and says so. See METHODOLOGY.md section 7.
- `-n, --requests` no longer overshoots its cap. Workers checked the sent counter and then sent, so concurrent workers could exceed the cap by about one message per connection (104 messages for `-n 101` with 40 connections). Each send is now claimed from a shared counter first, so a run sends at most the cap.

### Changed
- The live counters behind the progress bar, the TUI and the time series are split into cache-line-aligned shards, so workers on different CPU cores no longer contend for the same counters on every message. The final report is unchanged.
- Paced echo sends follow a fixed schedule instead of skipping missed ticks. A request that is late because an earlier reply was slow is sent as soon as that reply arrives, so a connection keeps `--rate` on average while the server keeps up. Achieved rates at high `--rate` values now match the requested rate.
- **Breaking for SLO gates:** in a paced echo run, `--max-p50`, `--max-p95`, `--max-p99` and `--max-p999` gate on response time instead of round-trip time, and the checks are named `Response time p99` and so on. A run whose server cannot keep up with `--rate` can now fail a latency gate it passed before. Unpaced runs still gate on round-trip time.
- Help and usage errors name the binary `wsblast` on every platform. On Windows they said `wsblast.exe`.
- A subcommand placed after load-test flags (`wsblast -c 2 mock`) now fails with a message saying subcommands go first, instead of "Invalid target URL 'mock'".
- CI runs on pinned runner images (`ubuntu-24.04`, `macos-15`, `windows-2025`) and `actions/checkout@v5`.

## [0.2.0] - 2026-10-24

v0.2.0 ships a smaller crate in which every feature works end to end. Several features that were developed on `main` after 0.1.1 were removed before release; their code is preserved on the [`archive/v0.1-full-features`](https://github.com/jephter-olamiposi/wsblast/tree/archive/v0.1-full-features) branch, and several of them will return, rebuilt, in later releases.

### Migrating from 0.1

- Every 0.1.1 flag still works. Two behaviours changed: `--duration` now includes the `--ramp-rate` ramp, and workers reconnect after a timeout, I/O error or server close. Pass `--no-reconnect` to end a worker on its first connection loss, as 0.1 did.
- New capabilities are Cargo features (`scenario`, `auth`, `graphql-ws`, `compare`), all enabled by default, so `cargo install wsblast` and the release binaries include them. `--features full` still works and is now the same as the default. Library users who want the 0.1 dependency set can use `default-features = false`.
- Library users: the library is now a small embedding API re-exported from the crate root, and its modules are private. Build a configuration with `LoadTestConfig::builder(url)`, which applies the same validation as the command line; `LoadTestConfig` fields are private and are read through getters. Set SLOs with `SloThresholds::new().with_max_p99(...)`. `ScenarioExecutor::run` now returns merged metrics in `ScenarioOutcome { metrics, phase_targets }`, and scenarios load with `ScenarioConfig::load` or `ScenarioConfig::from_yaml`. `Cli`, `WorkerSession`, `WorkerMetrics`, `LiveMetrics`, `PayloadConfig`, `AuthConfig`, `AggregatedMetrics::merge`, `Runner::run_and_collect_worker_metrics`, the comparison functions, the auth helpers and the TUI are no longer public. Public enums and the metric, SLO and scenario result structs are `#[non_exhaustive]`. `ConfigError::ZeroRate`, `WsBlastError::SloBreach` and `WorkerError::ChannelClosed` are gone, and a zero `--requests` limit is reported as `ConfigError::InvalidOption` instead of `ConfigError::InvalidThreshold`.
- If you built from `main` after 0.1.1 and enabled a removed feature (`distributed`, `otel`, `prometheus`, `capture`, `chaos`, `mock`, `compression`, `tracing-subscriber`), drop it from `--features` and remove its flags. Build the archive branch if you still need one of them.

### Added
- Man page and shell completions. `wsblast man` prints the wsblast(1) man page and `wsblast completions <bash|zsh|fish|powershell|elvish>` prints a completion script; both commands are hidden and leave the normal command line unchanged. The release archives ship them as `wsblast.1` and `completions/`.
- Scenario DSL with YAML multi-phase definitions, per-phase targets, actions (`Send`, `Wait`, `Ping`, `Close`), think time, and CLI URL precedence.
- Handshake authentication (`auth` feature) for static bearer tokens and API keys (`--auth-type static|apikey`).
- Apollo GraphQL WS subscription mode (`--mode graphql-ws`) with `connection_init` / `subscribe` lifecycle handling and legacy `graphql-ws` (`start` / `data`) support.
- Baseline comparison (`--compare`) with regression thresholds and `--fail-on-regression`.
- `--match <first|exact|json:POINTER>` selects how echo mode recognises the reply to a request so unsolicited server pushes no longer skew RTT; skipped frames are reported as `total_unsolicited_frames`.

### Changed
- `install.sh` falls back to `~/.local/bin` when sudo is missing or cannot run, and installs into `WSBLAST_INSTALL_DIR` when it is set.
- `scenario`, `compare`, `auth` and `graphql-ws` are default features. The prebuilt binaries, `cargo binstall wsblast` and `cargo install wsblast` now include `--scenario`, `--compare`, `--auth-type` and `--mode graphql-ws`. Only `scenario` adds a dependency (the YAML parser); `auth`, `graphql-ws` and `compare` add none, so they are on by default too.
- Scenario files are parsed with `serde_norway`, a maintained fork of the archived `serde_yaml`. The YAML syntax, unknown-key errors and their messages are unchanged. The implicit `serde_yaml` Cargo feature that the optional dependency used to create is gone; enable `scenario` instead.
- TLS uses rustls with the platform root certificates only. The `native-tls` backend, which linked OpenSSL and was the one actually used for `wss://` when both were enabled, is gone, so `cargo install wsblast` no longer needs `libssl-dev` or `pkg-config`, and the Docker image no longer ships `libssl3`. `SSL_CERT_FILE` and `SSL_CERT_DIR` override the trusted roots. A rejected server certificate is reported as a TLS handshake error.
- Metric constructors and aggregation are fallible (`WorkerMetrics::new`, `AggregatedMetrics::merge`); runner and executor APIs propagate metrics errors.
- Configuration errors that are not thresholds have accurate variants: `ConfigError::InvalidAuth` for authentication options, and `ConfigError::InvalidOption` for a zero `--requests` limit, a missing `--graphql-ws-operation`, GraphQL WS variables or extensions that are not a JSON object, and `--compare` without exactly two files. `ConfigError::InvalidThreshold` now only covers `--max-error-rate` and `--compare-threshold`.
- README documents feature flags, scenario targets, authentication lifetime, GraphQL WS and comparison workflows.
- The minimum supported Rust version is now 1.88 (`rust-version` in `Cargo.toml`), for the default build and for `--features full`. The locked dependency graph needs it: `comfy-table` 7.2 uses let chains, which Rust 1.88 stabilised, so the previously declared 1.85 no longer builds the crate. The Docker image and the GitLab CI example now use `rust:1.88`.

### Removed
- Distributed coordinator/worker execution over gRPC (`distributed` feature, `--distributed-mode` and the coordinator and worker flags), with its protobuf definitions and build script. Building the crate no longer needs `protoc`.
- OpenTelemetry OTLP export (`otel` feature, `--otel*` flags) and the Prometheus `/metrics` endpoint (`prometheus` feature, `--prometheus*` flags).
- Frame capture to PCAP-NG and JSON Lines (`capture` feature, `--capture*` flags).
- Chaos injection of latency, frame loss, corruption and forced closes (`chaos` feature, `--chaos*` flags).
- The record/replay mock server (`mock` feature, `wsblast mock` subcommands).
- JWT and OAuth2 authentication (`--auth-type jwt|oauth2`, `--auth-jwt-*` and `--auth-oauth2-*` flags), the unused `Authenticator` trait and authenticator structs, and `AuthConfig::Custom`.
- The `compression` feature and `--compression` flags, which never compressed anything because the transport does not implement RFC 7692.
- The `tracing-subscriber` feature.
- `ConfigError::ZeroRate`, `WsBlastError::SloBreach` and `WorkerError::ChannelClosed`, which were never constructed.
- The unused `base64`, `rand` and `sha2` dependencies.
- `--compression` now fails fast with `CompressionUnsupported` instead of silently running uncompressed.
- README documents feature flags, scenario targets, auth lifetime behavior, observability, chaos, capture, GraphQL, distributed limits, and mock/comparison workflows.
- Command-line usage errors (unknown flag, missing value, conflicting flags) now exit with code 1 instead of clap's default 2, so exit code 2 always means an SLO breach or a `--fail-on-regression` regression; `--help` and `--version` still exit with 0.

### Fixed
- GraphQL WS mode no longer records an `UnexpectedClose` error when the run ends before the server completes the subscription, and now sends the stop/complete message before the Close frame.
- Echo mode no longer counts a server Close frame as a received reply with an RTT sample; the Close ends the session (and triggers a reconnect unless `--no-reconnect` is set).
- A message timeout, I/O error or server close no longer ends a worker for the rest of the run: workers reconnect with exponential backoff (100ms doubling to 5s) and report `total_reconnects`; `--no-reconnect` restores the old behaviour.
- Echo-mode shutdown no longer waits up to `--message-timeout` for an in-flight reply: the receive wait races cancellation, and replies arriving after the test window are no longer recorded.
- Echo mode with `--ping-interval` no longer throttles unpaced load to one message per keepalive tick, and keepalive ticks no longer add extra data sends when `--rate` is set.
- `--duration` is now the total wall-clock run including the `--ramp-rate` ramp, so runs no longer last ramp plus duration and throughput is no longer diluted by ramp time; a warning is printed when the ramp cannot finish within the duration.
- Ctrl+C or SIGTERM during a scenario now stops the whole scenario and prints the report instead of only ending the current phase and starting the next one.
- Ctrl+C and SIGTERM during a `--ramp-rate` connection ramp now stop the run and print the report instead of killing the process; `Runner::run` and `Runner::run_and_collect_worker_metrics` share one code path.
- Latency SLOs (`--max-p50/p95/p99/p999`) no longer pass when no round trip was measured: the check fails with "no RTT samples", and latency SLOs are rejected at startup outside `--mode echo`.
- Single-phase runs no longer execute the load test twice.
- Remaining runtime `.unwrap()`/`.expect()` calls in metric initialization were replaced with typed errors.
- GraphQL WS now emits protocol-correct wire names (`connection_init`, `subscribe`, `next`, `complete`) instead of Rust variant names, and handles modern keepalives.
- Duration flags no longer panic on negative, NaN, infinite or overflowing values (`-d infs`, `--duration=-1s`); they fail with `InvalidDuration`, and the parser now accepts `ns` and fractional values for every unit (`1.5ms`, `0.5us`).
- Repeated `-H` flags with the same header name now send every value instead of keeping only the last one; a `-H` header still replaces a default handshake header of the same name (such as `Host`).
- Comparison reports print improvements with their real sign (throughput 100 to 150 shows `+50.00%`, not `-50.00%`); `improvements[].pct_change` in comparison JSON is now the signed change instead of its absolute value.
- `install.sh` downloads the release assets that are actually published (`wsblast-<target>.tar.gz`), verifies their `.sha256` checksum before installing, and fails with a clear message when the GitHub API lookup fails instead of assuming `v0.1.0`; releases now build `x86_64-apple-darwin` on `macos-latest` and add `aarch64-unknown-linux-gnu`.
- The Docker image builds again: the Dockerfile copies the whole crate (the manifest's `[[bench]]` target was missing), installs the OpenSSL headers `native-tls` needs, and builds only `--bin wsblast`; `.dockerignore` now excludes build output, VCS, CI, assets and local agent files.
- `error_rate` no longer counts a failed echo request twice: it is now failed operations over attempted operations, so a run where every request times out reports 1.0 instead of 0.5; errors that end an already-sent message (reply timeout, read error or close while the reply is awaited) are classified when recorded and add no second attempt.
- A scenario phase without a `target` no longer silently reuses the first phase's endpoint when neither `--url` nor a scenario-level `target` is set: validation rejects it and names the phase, the CLI > phase > scenario precedence is resolved for every phase, and text and Markdown reports list every executed phase's target (JSON adds an optional `phase_targets` array).
- Scenario files now reject unknown keys at the top level and in `metadata`, phases and actions, naming the misspelled key (`conections:` used to fall back silently to a default), and `Close` actions only accept the close codes an endpoint may send (1000-1003, 1007-1014, 3000-4999) instead of also accepting the reserved 1016-2999 range.
- Connection slots the ramp never reached before `--duration` ended are no longer reported as failed with no error recorded; they are counted in the new `total_connections_not_started` field, shown in text and Markdown reports when non-zero.
- With the progress bar enabled, `elapsed` no longer overshoots `--duration` by up to 100 ms: the progress loop wakes on cancellation instead of finishing its 100 ms sleep, so `elapsed` matches `--no-progress` runs.

## [0.1.1] - 2026-09-04

### Changed
- Streamlined documentation to adopt concise systems engineering documentation style.
- Updated crate packaging manifest with explicit asset exclusions.

## [0.1.0] - 2026-09-04

### Added
- **Core Engine & Architecture:**
  - High-performance, low-contention WebSocket load generation engine built with Tokio and `tokio-tungstenite`.
  - Multi-mode support: `echo` (request-response with microsecond RTT latency tracking), `stream` (unidirectional high-throughput pipe), and `listen` (passive subscriber).
  - Pacing and rate limiting per connection using `tokio::time::interval` with `MissedTickBehavior::Skip` to prevent burst storms.
  - Connection ramp pacing (`--ramp-rate`) to avoid kernel SYN queue saturation and thundering-herd issues during high-concurrency connection ramp-up.
  - Request limit enforcement (`-n, --requests`) allowing benchmarking by message count in addition to or instead of time-based duration.
  - Granular failure taxonomy mapping socket drops into DNS resolution, TCP connect, TLS handshake, HTTP 101 upgrade rejection (`4xx`/`5xx`), protocol violation, timeout, and unexpected close.
- **Latency & Telemetry:**
  - Task-local `HdrHistogram` recording (1 µs to 60s at 3 significant figures) ensuring zero mutex contention across concurrent workers.
  - Lock-free live atomic counters (`LiveMetrics`) providing real-time telemetry updates.
  - Microsecond-accurate monotonic timing via hardware `Instant` clocking.
- **Terminal UI & Reporting:**
  - Interactive live terminal dashboard (`--tui`) powered by `ratatui` with real-time throughput sparklines, connection meters, and active error monitors.
  - Automated Service Level Objective (SLO) gating for latency percentiles (`p50`, `p95`, `p99`, `p99.9`), maximum error rate, and minimum throughput.
  - Multi-format report emission: human-readable UTF-8 terminal tables, schema v1.0 JSON (`--format json`), and GitHub Flavored Markdown (`--format markdown`) for PR summaries and CI artifacts.
- **Transport & Security:**
  - Secure TLS WebSocket (`wss://`) support backed by `rustls` 0.23 with pre-installed `ring` crypto provider and system native root certificates.
  - Custom HTTP handshake headers (`-H, --header`) and subprotocol negotiation (`--subprotocol`).
  - RFC 6455 Ping/Pong keepalive handling across all worker execution modes.
- **Deployment & Tooling:**
  - Multi-stage minimal production `Dockerfile` with non-privileged execution (`USER 1000:1000`).
  - POSIX-compliant one-line installer script (`install.sh`) supporting macOS and Linux across x86_64 and arm64.
  - Automated cross-platform GitHub Actions release workflow producing standalone binaries for Linux, macOS (Apple Silicon & Intel), and Windows.
  - Standalone high-concurrency echo benchmark server (`examples/echo_server.rs`) with real-time throughput telemetry logging.
  - Comprehensive Criterion microbenchmark suite (`benches/metrics_benchmark.rs`).
