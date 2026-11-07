# Scenario reference

A scenario is a YAML file that describes one or more load phases. wsblast runs the phases in order and prints one merged report at the end. SLOs can gate the whole scenario and each phase on its own.

Scenario support is part of the default build (the `scenario` Cargo feature):

```bash
cargo install wsblast --locked
wsblast --scenario load.yaml --no-progress
```

From a checkout:

```bash
cargo run --release --example echo_server            # ws://127.0.0.1:9001
cargo run --release -- --scenario load.yaml --no-progress
```

Contents:

1. [File shape](#1-file-shape)
2. [Phase fields](#2-phase-fields)
3. [Actions](#3-actions)
4. [How a phase runs](#4-how-a-phase-runs)
5. [Target URL precedence](#5-target-url-precedence)
6. [Command-line flags with scenarios](#6-command-line-flags-with-scenarios)
7. [SLOs](#7-slos)
8. [The report](#8-the-report)
9. [Stopping a scenario](#9-stopping-a-scenario)
10. [Validation](#10-validation)
11. [Examples](#11-examples)

## 1. File shape

```yaml
version: "1.0"                    # required
target: "ws://127.0.0.1:9001"     # optional
metadata:                         # required
  name: "Checkout flow"           # required
  description: "Optional text"    # optional
  tags: ["checkout", "nightly"]   # optional
slo:                              # optional, checked on the whole scenario
  max_error_rate: 0.01
phases:                           # required, at least one
  - name: "warmup"
    duration: "10s"
    connections: 10
    slo:                          # optional, checked on this phase alone
      max_p99: "50ms"
```

| Field | Type | Required | Default | Rules |
| :--- | :--- | :--- | :--- | :--- |
| `version` | string | yes | | Must be `"1.0"`. Quote it: an unquoted `1.0` happens to be accepted, but `1` is read as `"1"` and rejected. |
| `target` | string | no | none | A `ws://` or `wss://` URL. Validated at load time even when the command line overrides it. |
| `metadata.name` | string | yes | | Must not be empty or only whitespace. Shown in the phase banner. |
| `metadata.description` | string | no | none | Not used by the run. |
| `metadata.tags` | list of strings | no | none | Not used by the run. |
| `slo` | SLO mapping | no | none | Thresholds checked on the merged metrics of every phase. See [section 7](#7-slos). |
| `data` | path | no | none | A data file whose columns payloads read as `{{data.COLUMN}}`, relative to the scenario file. `--data` replaces it. See [templates](templates.md#2-data-files). |
| `phases` | list of phases | yes | | At least one. Phase names must be unique. |

Unknown keys are rejected at every level: the top level, `metadata`, each phase, each `slo` mapping and each action. A misspelled field fails validation with its name and the list of valid keys, so it cannot silently fall back to a default:

```text
Error: Configuration error: Invalid scenario: load.yaml: phases[0]: unknown field `conections`, expected one of `name`, `target`, `duration`, `connections`, `ramp_rate`, `stages`, `rate_per_conn`, `payload`, `think_time`, `actions`, `slo`, `chaos` at line 10 column 5
```

## 2. Phase fields

| Field | Type | Required | Default | Meaning |
| :--- | :--- | :--- | :--- | :--- |
| `name` | string | yes | | Unique, not empty. Used by `--phase` and in the banner. |
| `target` | string | no | none | `ws://` or `wss://` URL for this phase. See [section 5](#5-target-url-precedence). |
| `duration` | duration | yes, unless `stages` is set | | Total wall-clock time of the phase, including its ramp. Must be greater than zero. |
| `connections` | integer | yes, unless `stages` or `arrival_rate` is set | | Worker connections for this phase. Must be greater than zero. |
| `ramp_rate` | integer | no | `0` | Workers started per second. `0` starts all of them at once. |
| `stages` | list of stages, or a string | no | none | A staged load profile in place of `connections`, `duration` and `ramp_rate`, which cannot be set with it. See [section 4.4](#44-staged-phases). |
| `arrival_rate` | number | no | none | New sessions per second in place of `connections`, `ramp_rate` and `stages`, which cannot be set with it. Needs `session_duration` and `duration`. See [section 4.6](#46-arrival-rate-phases). |
| `session_duration` | duration | with `arrival_rate` | | How long each session stays connected, handshake included. |
| `max_connections` | integer | no | none | With `arrival_rate`, the most sessions open at once; arrivals while this many are open are skipped. |
| `rate_per_conn` | integer | no | `0` | Messages per second per connection. `0` is unthrottled. In an action script it paces every individual send; in echo mode those sends follow a fixed schedule and record response time ([section 4.3](#43-phases-with-actions)). |
| `payload` | string | no | see below | Text payload for this phase. Supports `{{timestamp}}`, `{{worker_id}}` and `{{seq}}`. |
| `think_time` | duration | no | `0` | Pause after each `Send` action, or after each message when the phase has no actions. See [section 4](#4-how-a-phase-runs). |
| `actions` | list of actions | no | `[]` | Script each connection runs. Without actions the phase runs the `--mode` loop. |
| `slo` | SLO mapping | no | none | Thresholds checked on this phase's own metrics. See [section 7](#7-slos). |
| `chaos` | chaos mapping | no | the `--chaos-*` flags | Client-side faults this phase injects instead of those of the command line. See [section 4.5](#45-chaos). Needs the `chaos` feature (on by default). |

**Durations** are strings with a unit: `ns`, `us` (or `µs`), `ms`, `s`, `m`, `h`. Fractions are allowed (`1.5s`, `0.5ms`). A bare integer is seconds. Negative, infinite and out-of-range values are rejected.

**Payload resolution** for a message, first match wins:

1. The `payload` of the `Send` action.
2. The phase `payload`.
3. The default payload: `{"source":"wsblast","ts":"{{timestamp}}","seq":{{seq}}}`, or the binary frame `wsblast-payload` when `--binary` is given.

Scenario payloads are always text frames. `--binary` only changes the default payload. A payload that contains `{{` is rendered for every message; any other payload is sent as is.

| Placeholder | Value |
| :--- | :--- |
| `{{timestamp}}` | Current Unix time in milliseconds (system clock). |
| `{{worker_id}}` | Index of the connection within the phase, starting at 0. |
| `{{seq}}` | Per-connection message counter, starting at 0 and reset on every new connection. |

## 3. Actions

Each action is a map with a `type` key. Type names are case-sensitive: `Send`, `Wait`, `Ping`, `Close`.

| Type | Fields | Default | Behaviour |
| :--- | :--- | :--- | :--- |
| `Send` | `payload` (string, optional), `count` (integer, optional), `expect` (mapping, optional), `capture` (mapping, optional), `name` (string, optional), `slo` (mapping, optional) | `count: 1` | Sends `count` messages back to back, each one paced by `rate_per_conn` when it is set. In echo mode each message waits for its reply and records an RTT sample, and a response time sample when `rate_per_conn` is set. After the whole burst the worker sleeps `think_time`. `count` must be greater than zero. In echo mode, `expect` checks every reply and `capture` sets variables from it; see [templates, section 3](templates.md#3-values-from-replies-and-reply-checks). `name` reports the action's messages on their own, and `slo` gates on them; see [section 7.3](#73-named-actions). |
| `Wait` | `duration` (duration, required) | | Sleeps for `duration`. Must be greater than zero. |
| `Ping` | `interval` (duration, required) | | Sends one WebSocket Ping frame (payload `hb`), then sleeps for `interval`. It is a single ping followed by a pause, not a repeating keepalive. Must be greater than zero. Pings are not counted as messages. |
| `Close` | `code` (integer, optional), `reason` (string, optional) | `code: 1000`, `reason: ""` | Sends a Close frame with this code and reason, then ends the connection for the rest of the phase. The worker does not reconnect. |

### Close code rules

`code` must be one an endpoint is allowed to send:

| Codes | Accepted | Why |
| :--- | :--- | :--- |
| 1000-1003 | yes | Defined by RFC 6455 (normal closure, going away, protocol error, unsupported data). |
| 1004-1006 | no | 1004 is reserved; 1005 and 1006 must never be sent in a Close frame. |
| 1007-1014 | yes | 1007-1011 are defined by RFC 6455, 1012-1014 are registered with IANA. |
| 1015 | no | Must never be sent in a Close frame. |
| 1016-2999 | no | Reserved by RFC 6455 for future protocol use. |
| 3000-3999 | yes | Registered with IANA for libraries, frameworks and applications. |
| 4000-4999 | yes | Private use by applications. |

Any other value fails validation:

```text
Error: Configuration error: Invalid scenario: close code 2000 cannot be sent; use 1000-1003, 1007-1014 or 3000-4999
```

Use 1000 (normal closure), 1001 (going away), or an application code in 4000 to 4999.

## 4. How a phase runs

### 4.1 Phase lifecycle

1. wsblast prints a banner on stderr, for example `▶ Scenario 'Chat session' phase 1/2: 'warmup' (5 connections for 3s)`, or `→ Scenario 'Rollout' phase 2/3: 'ramp' (stages 30s:100, 1m:100, 30s:0)` for a staged phase.
2. It starts `connections` workers, `ramp_rate` per second, or follows the phase's `stages`.
3. The phase ends when its `duration` (the sum of its stage durations for a staged phase) has elapsed (counted from the first worker, ramp included), when the `--requests` cap is reached, or on Ctrl+C. Every worker sends a Close frame and stops.
4. wsblast waits 500 ms and starts the next phase with new connections.

A phase always lasts its full `duration`, even if every connection has finished its script with a `Close` action or has given up with `--no-reconnect`.

### 4.2 Phases without actions

The phase runs the normal session loop for `--mode` (echo by default) with the phase's `connections`, `ramp_rate`, `rate_per_conn` and `payload`. `think_time` is applied:

| Mode | When `think_time` applies |
| :--- | :--- |
| `echo`, `socketio` | After each reply (an ack in `socketio` mode), before the next request. |
| `stream` | After each send. |
| `listen`, `graphql-ws` | Never. |

### 4.3 Phases with actions

Each connection runs the action list from the top, and starts again from the first action after the last one. It keeps looping until the phase ends or a `Close` action runs.

- Before every action and before every message of a `Send` burst, the worker checks for cancellation and for the `--requests` cap.
- If `rate_per_conn` is set in echo mode, the messages follow a fixed schedule, as `--rate` does in echo mode: message `n` of a connection is due `n / rate_per_conn` seconds after the connection opened, is never sent before it is due, and is sent as soon as the previous reply arrives when that reply was late. Every reply records response time, measured from the message's due time, so a slow server shows up as response time the way it does for `--rate` ([METHODOLOGY.md section 4](../METHODOLOGY.md#4-timing)). A `Wait` or `Ping` action and the `think_time` after a `Send` burst are deliberate pauses: the schedule restarts when each one ends, so a pause is never counted as lateness.
- If `rate_per_conn` is set in stream or listen mode, every message waits for the next tick of a per-connection interval of `1 / rate_per_conn` seconds. Missed ticks are skipped, not sent in a burst.
- In echo mode, a message that gets no reply within `--message-timeout` records a `timeout` error and drops the connection. Unless `--no-reconnect` is set, the worker reconnects with backoff (100 ms doubling to 5 s) and the script starts again from the first action on the new connection. A server Close frame has the same effect, without an error.
- In `stream` and `listen` mode a script sends without waiting for replies and does not read incoming frames, so the report shows 0 messages received. Use echo mode when the script needs replies.
- `--ping-interval` keepalives are not sent while a script runs. Add a `Ping` action instead.

### 4.4 Staged phases

A phase with `stages` runs exactly like a run with `--stages`: over each stage the number of open connections moves linearly from the previous stage's target (0 for the first stage) to the stage's `target`, and when the count falls the most recently opened connections are closed first. The phase lasts the sum of the stage durations. Its `rate_per_conn`, `payload`, `think_time` and `actions` apply to every connection it opens, and a connection that a falling stage closes ends its session or script there.

Give the stages either as a list of mappings or as one string in the syntax of `--stages`; both forms are equivalent:

```yaml
phases:
  - name: "ramp"
    rate_per_conn: 20
    stages:
      - duration: "30s"
        target: 100
      - duration: "1m"
        target: 100
      - duration: "30s"
        target: 0

  - name: "ramp-again"
    rate_per_conn: 20
    stages: "30s:100,1m:100,30s:0"
```

| Stage field | Type | Required | Meaning |
| :--- | :--- | :--- | :--- |
| `duration` | duration | yes | How long the stage lasts. Must be greater than zero. |
| `target` | integer | yes | Connections open at the end of the stage. `0` is allowed, but at least one stage needs a target above 0. |

`stages` replaces `connections`, `duration` and `ramp_rate`; setting any of them in the same phase is an error that names the phase and the keys. The phase's configured connection count, shown as `connections` of the phase in the JSON report's `phases`, is its highest target. Its `total_connections_attempted` counts every connection the phase opened, so a profile that ramps down and up again attempts more connections than its peak. No slot of a staged phase is reported as not started. See METHODOLOGY.md, section 2.1.1.

### 4.5 Chaos

A phase's `chaos` mapping injects client-side faults into that phase, with the keys and values of the `--chaos-*` flags:

| Key | Type | Default | Meaning |
| :--- | :--- | :--- | :--- |
| `delay` | duration, or `DURATION:JITTER` | none | Wait this long before every data send, plus a value drawn uniformly from 0 to `JITTER`. |
| `drop` | number, 0 to 100 | `0` | Percentage of data sends skipped. They are not counted as sent. |
| `corrupt` | number, 0 to 100 | `0` | Percentage of data sends with one byte changed. `drop` and `corrupt` add up to at most 100. |
| `close` | number, 0 to 100 | `0` | Percentage of replies (of sends in stream mode) after which the client closes the connection; the worker reconnects. |
| `close_mode` | `abrupt` or `graceful` | `abrupt` | `abrupt` closes the TCP connection without a Close frame; `graceful` sends a Close frame with code 1001 first. |
| `read_pause` | duration, or `DURATION:JITTER` | none | A slow consumer: after every received data frame, wait this long, plus a value drawn uniformly from 0 to `JITTER`, before reading the next. Works in every mode, `listen` and `graphql-ws` included. |
| `seed` | integer | `--chaos-seed`, else picked per run | Seed of the phase's decisions. |

```yaml
phases:
  - name: "flaky clients"
    duration: "1m"
    connections: 200
    rate_per_conn: 10
    chaos:
      delay: "5ms:20ms"
      drop: 1
      close: 0.5
      close_mode: graceful
      seed: 42
```

- The mapping replaces the command-line `--chaos-*` options for that phase as a whole: keys it leaves out are off, not taken from the flags. `chaos: {}` runs the phase without faults. A phase without a `chaos` key uses the flags.
- A phase without a `seed` uses `--chaos-seed` when it is given and otherwise picks its own seed. Every phase numbers its connection slots from 0, so phases with the same seed make the same decisions on the same slots.
- Injected events are never errors and add no latency sample by themselves; [METHODOLOGY.md section 13](../METHODOLOGY.md#13-chaos-injection) defines their effect on every metric. The phase's own metrics in the JSON report carry its `chaos` counts, and the merged `metrics.chaos` adds up the counts of every phase, with the seed of the first phase that injected faults.
- Chaos acts on data sends: in `listen` and `graphql-ws` mode a phase may use it only with an action script.

### 4.6 Arrival-rate phases

A phase with `arrival_rate` runs like `--arrival-rate`: a new session starts every `1 / arrival_rate` seconds for `duration`, and each one closes after `session_duration`. Its `rate_per_conn`, `payload`, `think_time` and `actions` apply to every session, so a session can log in, subscribe, wait and leave on its own schedule while new users keep arriving:

```yaml
phases:
  - name: "steady"
    connections: 200
    duration: "1m"
  - name: "churn"
    arrival_rate: 50          # new sessions per second
    session_duration: "20s"   # about 1,000 open at once
    max_connections: 2000
    duration: "2m"
    actions:
      - type: Send
        name: login
        payload: '{"op":"login"}'
      - type: Wait
        duration: "15s"
```

Each phase reports its arrivals under `metrics.arrivals` in its entry of `phases`, and the phase table shows its rate and session length as its load. The scenario's merged `metrics.arrivals` adds up the sessions started and skipped by every arrival phase. See METHODOLOGY.md, section 2.1.2.

## 5. Target URL precedence

For each phase, the target is the first of:

1. The URL on the command line (positional `URL` or `-u, --url`).
2. The phase's `target`.
3. The scenario's `target`.

The rule is applied to every phase on its own; a phase never inherits another phase's target. If none of the three is set for a phase that will run, wsblast exits with code 1 before opening any connection and names the phase:

```text
Error: Configuration error: Invalid scenario: phase 'spike': no target URL; set `target` on this phase or at the top level of the scenario, or pass a URL on the command line
```

Set a top-level `target` when phases should share an endpoint. With `--phase`, only the selected phase needs a target.

## 6. Command-line flags with scenarios

| Flag | With `--scenario` |
| :--- | :--- |
| `--phase NAME` | Runs only the named phase. An unknown name is an error that lists the available phases. Requires `--scenario`. |
| `URL`, `-u, --url` | Overrides every phase and scenario target. |
| `-c`, `-d`, `-r`, `-p`, `--payload-file` | Not allowed. The argument parser rejects them and exits with code 1. Set them per phase instead. |
| `--stages` | Not allowed; wsblast exits with code 1. Set `stages` on a phase instead ([section 4.4](#44-staged-phases)). |
| `--ramp-rate` | Ignored. Each phase's `ramp_rate` is used. |
| `--tui` | Shows the phase in progress (name, number and its own SLOs) and the pass or fail of every finished phase. The per-phase header lines on stderr are not printed. |
| `--mode` | Applies to every phase. |
| `-n, --requests` | One cap for the whole scenario. Once reached, the remaining phases are cancelled as soon as they start. |
| `--binary` | Changes only the default payload (see [section 2](#2-phase-fields)). |
| `--connect-timeout`, `--message-timeout`, `--ping-interval`, `-H`, `--subprotocol`, `--no-reconnect`, authentication flags | Apply to every phase. |
| SLO flags | Checked on the merged metrics of the whole scenario, together with the scenario's `slo` mapping. A flag overrides the scenario's threshold of the same name. See [section 7](#7-slos). |
| `--format`, `-o`, `--no-progress` | Apply to the merged report. |

## 7. SLOs

An `slo` mapping can be set at the top level of the file and on any phase. It takes the thresholds of the SLO flags, under the flag names with underscores:

| Key | Type | Flag | Passes when |
| :--- | :--- | :--- | :--- |
| `max_p50` | duration | `--max-p50` | The gating latency's p50 is at most this value. |
| `max_p95` | duration | `--max-p95` | The same, at p95. |
| `max_p99` | duration | `--max-p99` | The same, at p99. |
| `max_p999` | duration | `--max-p999` | The same, at p99.9. |
| `max_error_rate` | number, 0 to 1 | `--max-error-rate` | `error_rate` is at most this value. |
| `min_throughput` | number, messages per second | `--min-throughput` | `throughput_msg_per_sec` is at least this value. |
| `max_generator_lag` | duration | `--max-generator-lag` | The p99 wake-up lag of the load generator itself is at most this value ([METHODOLOGY.md section 14](../METHODOLOGY.md#14-load-generator-saturation)). Allowed in every mode. |

Every key is optional, and a threshold that is not set is not checked. Values follow the rules of the flags: durations use the syntax of [section 2](#2-phase-fields), `max_error_rate` must be between 0 and 1, and latency thresholds need `--mode echo` (the default), because no other mode measures round trips.

```yaml
version: "1.0"
target: "ws://127.0.0.1:9001"
metadata:
  name: "Gated rollout"
slo:                       # the whole scenario
  max_error_rate: 0.01
phases:
  - name: "warmup"         # no SLO of its own
    duration: "10s"
    connections: 20
  - name: "spike"
    duration: "30s"
    connections: 200
    rate_per_conn: 20
    slo:                   # this phase alone
      max_p99: "50ms"
      max_error_rate: 0.001
      min_throughput: 3500
```

### 7.1 What each SLO is checked on

- **A phase `slo`** is checked on that phase's own metrics: the workers of that phase only, over that phase's elapsed time. A spike that breaches its budget fails even when the quieter phases around it would bring the merged numbers under it.
- **The top-level `slo` and the SLO flags** are checked on the merged metrics of every executed phase, the same numbers the report's main tables show.
- **When the file and the command line set the same threshold, the command line wins.** Each flag replaces only the top-level threshold of the same name, so `--max-p99 20ms` with a file that sets `max_p99: 50ms` and `max_error_rate: 0.01` gates on a p99 of 20 ms and an error rate of 1%. Flags never change a phase's `slo`.

Latency thresholds gate on the latency a single run would gate on: response time when the phase is paced echo (`rate_per_conn` set, and either actions or no think time), RTT otherwise. Each phase chooses on its own, so in one scenario a paced phase's check reads `Response time p99` and an unpaced phase's `Latency p99`. The merged metrics gate on response time when any executed phase recorded it. A latency check with no samples fails, as it does on the command line.

### 7.2 Exit code and check names

The run exits with code 2 when any check fails: a phase's, the top-level `slo`'s or a flag's. All checks are listed in one SLO table: first those of the whole scenario, then each phase's in run order, prefixed with the phase name:

```text
│ Error Rate          ┆ <= 1.00%  ┆ 0.00%   ┆ PASSED │
│ spike: Latency p99  ┆ <= 50ms   ┆ 61.2ms  ┆ FAILED │
```

With `--phase`, only the selected phase runs, so only its `slo` is checked, together with the top-level `slo` and the flags on that phase's metrics.

### 7.3 Named actions

A scenario's latency numbers mix every message it sends, so a slow login hides among a hundred fast subscribes. Give a `Send` action a `name` to report its messages on their own, and an `slo` to gate on them:

```yaml
version: "1.0"
target: "ws://127.0.0.1:9001"
metadata:
  name: "Login, then subscribe"
phases:
  - name: "session"
    connections: 200
    duration: "2m"
    actions:
      - type: Send
        name: login
        payload: '{"op":"login"}'
        slo: { max_p99: "200ms", max_error_rate: 0.001 }
      - type: Send
        name: subscribe
        count: 20
        payload: '{"op":"subscribe","id":{{seq}}}'
        slo: { max_p99: "20ms" }
      - type: Wait
        duration: "5s"
```

- A name collects the messages of every action with that name, in every phase and on every connection. Actions in different phases, or two actions of one phase, can share a name.
- A named action counts its messages, the errors while it runs (reply timeouts, failed checks, failed writes and connections that close while it waits for a reply) and the latency of its replies: response time when the phase is paced (`rate_per_conn`), RTT otherwise. The run's own numbers still include every message, named or not.
- An action's `slo` takes `max_p50`, `max_p95`, `max_p99`, `max_p999` and `max_error_rate`, and each name may have one `slo`, on any of its actions. Its checks are listed after the phase checks as `Action login: latency p99` (`response time p99` when paced) and `Action login: error rate`, and a failed one makes the run exit with 2.
- A named action that sends nothing fails its checks, with "no RTT samples" or "no messages". With `--phase`, only the names that the selected phase sends are checked.
- Names are 1 to 64 characters, without control characters. Latency thresholds need echo mode, as a phase's do.

## 8. The report

All phases are merged into one report: the main tables show the metrics of every executed phase together. The JSON report also lists every phase with its own metrics and SLO result under `phases`, and the HTML report shows a Phases section with one row per phase and the SLO checks of every phase that has its own.

The text and Markdown reports add a phase table when at least one phase sets its own `slo`, `stages` or `arrival_rate`, so the reports of scenarios with none of them look as they always did:

| Column | Value |
| :--- | :--- |
| Phase, Target | The phase name and the endpoint it connected to. |
| Load | The phase's connection count, its stages as `DURATION:TARGET` pairs for a staged phase, or its arrival rate and session length (`50/s, sessions of 20s`). |
| Throughput (msg/s), Error Rate | The phase's own values. |
| Gating p99 | The p99 the phase's latency SLOs gate on, with its basis (`RTT` or `response time`); `-` without samples. |
| SLO | `PASSED`, `FAILED`, or `-` for a phase without its own `slo`. |

| Report field | Value for a scenario |
| :--- | :--- |
| `connections` | Same as `metrics.total_connections_attempted`: the sum of `connections` over the executed phases, where a staged phase counts every connection it opened. |
| `duration_secs`, `metrics.elapsed` | Sum of each phase's elapsed time. The 500 ms pauses are not included. |
| `target_url` | Target of the first executed phase. |
| `phase_targets` | JSON only: one `{"phase", "target_url"}` entry per executed phase, in run order. The text and Markdown reports show the same list in place of the single target. |
| `phases` | JSON only: one entry per executed phase, in run order, with `name`, `target_url`, `connections` (the highest target for a staged phase), `stages` (staged phases only: `{"duration": {"secs", "nanos"}, "target"}` per stage), `metrics` (the phase's own metrics in the format of the top-level `metrics`, with a time series that starts at the phase start) and `slo` (the phase's own checks, without the prefix). |
| `slo` | Every check of the run, as described in [section 7.2](#72-exit-code-and-check-names). |
| `metrics.total_connections_attempted` | Same as `connections`. |
| Latency histograms | One distribution over the samples of every phase. |

When an action has a `name`, every format adds a By action table after the latency table, with each name's messages, errors, error rate and latency percentiles, and the JSON report lists the same values under `metrics.actions` ([section 7.3](#73-named-actions)).

The counters in `phases` add up to those of `metrics`. `wsblast render` reads `phases` back, so a rendered report shows the phase table and the Phases section too.

## 9. Stopping a scenario

Ctrl+C or SIGTERM stops the current phase and skips all remaining phases. wsblast then prints the merged report for what ran and exits with the normal exit code (0, or 2 if an SLO failed). Phases that did not run are not listed and their SLOs are not checked. A local run of example B below, interrupted after 2 s, reported `duration_secs` of 2.0, 10 connections (only the first phase) and exit code 0.

## 10. Validation

The whole file is validated before any connection is opened, including phases that `--phase` will not run. All of these fail with exit code 1:

| Problem | Message (after `Error: Configuration error: Invalid scenario:`) |
| :--- | :--- |
| `version` is not `"1.0"` | `unsupported version '2.0', expected '1.0'` |
| Empty `metadata.name` | `metadata.name must not be empty` |
| No phases | `scenario must define at least one phase` |
| Empty or duplicate phase name | `phase name must not be empty`, `duplicate phase name 'warmup'` |
| `connections: 0` | `phase 'warmup': connections must be greater than zero` |
| Zero `duration` | `phase 'warmup': duration must be greater than zero` |
| No `connections` or `duration`, and no `stages` | ``phase 'warmup': missing field `duration`; give `connections` and `duration`, or `stages` `` |
| `stages` with `connections`, `duration` or `ramp_rate` | ``phase 'ramp': `stages` cannot be combined with `connections`, `duration`; the stages set the connection count, the duration and the ramp`` |
| Empty `stages` | ``phase 'ramp': stages must list at least one stage, such as `{duration: 30s, target: 100}` `` |
| A stage with a zero duration | `phase 'ramp': stage 2 has a zero duration; every stage needs a duration greater than zero` |
| Every stage target is 0 | `phase 'ramp': every stage target is zero; at least one stage needs connections` |
| Unparsable stage | `phase 'ramp': stages: Configuration error: Invalid option: invalid stage '10s=5': expected DURATION:CONNECTIONS, such as 30s:100`, or `phase 'ramp': stage 1: duration: Configuration error: Invalid duration ...` for the list form |
| Unparsable duration | `phase 'warmup': Configuration error: Invalid duration 'abc': expected a non-negative number with a unit of ...` |
| Missing required field | ``load.yaml: phases[0]: missing field `name` at line 6 column 5`` |
| Unknown or misspelled key | ``load.yaml: phases[0]: unknown field `conections`, expected one of `name`, `target`, ... at line 10 column 5`` |
| Unknown action type | ``load.yaml: phases[0].actions[0].type: unknown variant `send`, expected one of `Send`, `Wait`, `Close`, `Ping` `` |
| `count: 0` | `phase 'warmup': send action count must be greater than zero` |
| Zero `Wait` or `Ping` duration | `phase 'warmup': wait action duration must be greater than zero` |
| Forbidden or reserved close code | `close code 2000 cannot be sent; use 1000-1003, 1007-1014 or 3000-4999` |
| `slo` on a `Send` without `name` | `phase 'session': slo on an action needs a name: the thresholds are checked on the named action's messages` |
| `min_throughput` or `max_generator_lag` in an action `slo` | `phase 'session': action 'login': slo: an action SLO takes max_p50, max_p95, max_p99, max_p999 and max_error_rate; min_throughput and max_generator_lag apply to a phase or the scenario` |
| A second `slo` for one name | `phase 'session': action 'login': slo: action 'login' already has an slo; give it one, on any of its actions` |
| An empty, long or control-character action name | `phase 'session': action name '' must be 1 to 64 characters, without control characters` |
| `arrival_rate` with `connections`, `ramp_rate` or `stages` | ``phase 'churn': `arrival_rate` cannot be combined with `connections`; the arrival rate sets how many sessions are open`` |
| `arrival_rate` without `session_duration` | ``phase 'churn': `arrival_rate` needs `session_duration`, how long each session stays connected`` |
| `session_duration` or `max_connections` without `arrival_rate` | ``phase 'churn': `session_duration` and `max_connections` need `arrival_rate` `` |
| `arrival_rate` not above 0, zero `session_duration` or `max_connections` | `phase 'churn': arrival_rate must be greater than zero and at most 1000000 sessions per second, received 0` |
| Unknown `slo` key | ``load.yaml: phases[0].slo: unknown field `max_p99_ms`, expected one of `max_p50`, `max_p95`, `max_p99`, `max_p999`, `max_error_rate`, `min_throughput`, `max_generator_lag` at line 9 column 7`` |
| Unparsable SLO duration | `phase 'spike': slo: max_p99: Configuration error: Invalid duration 'fast': ...` (`slo: max_p99: ...` at the top level) |
| `max_error_rate` outside 0 to 1 | `phase 'spike': slo: Configuration error: Invalid threshold specification: max_error_rate must be between 0.0 and 1.0, received 1.5` |
| `chaos` percentage outside 0 to 100, or `drop` and `corrupt` adding up to more than 100 | `phase 'flaky': chaos: Configuration error: Invalid option: chaos drop percentage must be between 0 and 100, received 150` |
| Unparsable `chaos` delay | `phase 'flaky': chaos: Configuration error: Invalid duration 'soon': ...` |
| `chaos` with a send-side key (`delay`, `drop`, `corrupt`, `close`) in a `listen` or `graphql-ws` phase without actions | `Unsupported option combination: phase 'watch': chaos injection needs data sends to act on; Listen mode sends none, use --mode echo or stream, or an action script (--chaos-read-pause works in every mode)` (exit code 1, when the phase will run) |
| `--phase` names a missing phase | `phase 'nope' not found; available phases: soak, spike` |
| A phase that will run has no target from the command line, the phase or the scenario | ``phase 'spike': no target URL; set `target` on this phase or at the top level of the scenario, or pass a URL on the command line`` |

An invalid URL in any `target` fails with `Invalid target URL` or `Unsupported WebSocket scheme`.

A latency threshold in the top-level `slo`, or in the `slo` of a phase that will run, fails with exit code 1 when `--mode` is not echo:

```text
Error: Configuration error: Unsupported option combination: phase 'spike': slo: latency SLOs (max_p50/max_p95/max_p99/max_p999) require --mode echo; Stream mode does not measure round-trip time
```

## 11. Examples

Both examples target the bundled echo server. Start it first:

```bash
cargo run --release --example echo_server     # listens on ws://127.0.0.1:9001
```

### Example A: warmup, then a scripted session

`chat-session.yaml`:

```yaml
version: "1.0"
target: "ws://127.0.0.1:9001"
metadata:
  name: "Chat session"
  description: "Warm up with paced echo traffic, then run a scripted chat session"
  tags: ["chat", "echo"]

phases:
  # No actions: the phase runs the --mode loop (echo by default).
  - name: "warmup"
    duration: "3s"
    connections: 5
    ramp_rate: 5
    rate_per_conn: 20
    payload: '{"type":"hello","worker":{{worker_id}},"seq":{{seq}}}'

  # An action script. Each connection runs it once, then closes.
  - name: "chat"
    duration: "5s"
    connections: 20
    ramp_rate: 10
    think_time: "50ms"
    actions:
      - type: Send
        payload: '{"type":"join","worker":{{worker_id}}}'
      - type: Send
        count: 5
        payload: '{"type":"msg","seq":{{seq}},"ts":{{timestamp}}}'
      - type: Wait
        duration: "200ms"
      - type: Ping
        interval: "100ms"
      - type: Close
        code: 1000
        reason: "done"
```

```bash
cargo run --release -- --scenario chat-session.yaml --no-progress
```

What happens:

- `warmup`: 5 connections start over 0.8 s and each sends 20 echo requests per second until 3 s have passed.
- `chat`: 20 connections start over 1.9 s. Each sends a join message, sleeps 50 ms, sends 5 messages, sleeps 50 ms, waits 200 ms, sends one ping, waits 100 ms and closes with code 1000. That is 6 messages per connection. The phase then idles until its 5 s are over.

A local run printed both phase banners, exited with 0, and reported 25 connections attempted and established, 380 messages sent and received, 380 RTT samples, 0 reconnects, 0 errors and `duration_secs` of 8.0. The 120 `chat` messages (20 x 6) match the script; the other 260 come from `warmup`.

### Example B: repeating script and per-phase targets

`soak.yaml`:

```yaml
version: "1.0"
metadata:
  name: "Soak with spike"
  description: "Repeating script at a fixed per-connection rate, then a short spike"

phases:
  - name: "soak"
    target: "ws://127.0.0.1:9001"
    duration: "6s"
    connections: 10
    ramp_rate: 5
    rate_per_conn: 50
    payload: '{"op":"tick","seq":{{seq}}}'
    actions:
      - type: Send
        count: 10
      - type: Wait
        duration: "100ms"

  - name: "spike"
    target: "ws://127.0.0.1:9001"
    duration: "3s"
    connections: 40
    rate_per_conn: 100
    payload: '{"op":"spike","seq":{{seq}}}'
```

```bash
# Both phases
cargo run --release -- --scenario soak.yaml --no-progress

# Only the spike phase, gated on the flags
cargo run --release -- --scenario soak.yaml --phase spike \
  --no-progress --max-p99 50ms --max-error-rate 0.01

# Same file against another server: the command-line URL overrides both phase targets
cargo run --release -- --scenario soak.yaml --url wss://staging.example.com/ws
```

What happens:

- `soak`: the script has no `Close`, so each connection repeats it for the whole phase: 10 messages paced at 50 per second (about 200 ms), then a 100 ms pause.
- `spike`: no actions, so it runs the echo loop at 100 messages per second per connection, all 40 connections at once.

Local runs exited with 0. Both phases together reported 50 connections, about 13,800 messages (one run: 13,837 sent, 13,836 received; the difference is the request in flight when the phase ended), 0 errors and `duration_secs` of 9.0. `--phase spike` with the two SLO flags reported 40 connections, 12,000 messages, a p99 of 370 µs, both checks passed, and `duration_secs` of 3.0. Exact counts vary from run to run.
