# Scenario reference

A scenario is a YAML file that describes one or more load phases. wsblast runs the phases in order and prints one merged report at the end.

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
7. [The report](#7-the-report)
8. [Stopping a scenario](#8-stopping-a-scenario)
9. [Validation](#9-validation)
10. [Examples](#10-examples)

## 1. File shape

```yaml
version: "1.0"                    # required
target: "ws://127.0.0.1:9001"     # optional
metadata:                         # required
  name: "Checkout flow"           # required
  description: "Optional text"    # optional
  tags: ["checkout", "nightly"]   # optional
phases:                           # required, at least one
  - name: "warmup"
    duration: "10s"
    connections: 10
```

| Field | Type | Required | Default | Rules |
| :--- | :--- | :--- | :--- | :--- |
| `version` | string | yes | | Must be `"1.0"`. Quote it: an unquoted `1.0` happens to be accepted, but `1` is read as `"1"` and rejected. |
| `target` | string | no | none | A `ws://` or `wss://` URL. Validated at load time even when the command line overrides it. |
| `metadata.name` | string | yes | | Must not be empty or only whitespace. Shown in the phase banner. |
| `metadata.description` | string | no | none | Not used by the run. |
| `metadata.tags` | list of strings | no | none | Not used by the run. |
| `phases` | list of phases | yes | | At least one. Phase names must be unique. |

Unknown keys are rejected at every level: the top level, `metadata`, each phase and each action. A misspelled field fails validation with its name and the list of valid keys, so it cannot silently fall back to a default:

```text
Error: Configuration error: Invalid scenario: load.yaml: phases[0]: unknown field `conections`, expected one of `name`, `target`, `duration`, `connections`, `ramp_rate`, `rate_per_conn`, `payload`, `think_time`, `actions` at line 10 column 5
```

## 2. Phase fields

| Field | Type | Required | Default | Meaning |
| :--- | :--- | :--- | :--- | :--- |
| `name` | string | yes | | Unique, not empty. Used by `--phase` and in the banner. |
| `target` | string | no | none | `ws://` or `wss://` URL for this phase. See [section 5](#5-target-url-precedence). |
| `duration` | duration | yes | | Total wall-clock time of the phase, including its ramp. Must be greater than zero. |
| `connections` | integer | yes | | Worker connections for this phase. Must be greater than zero. |
| `ramp_rate` | integer | no | `0` | Workers started per second. `0` starts all of them at once. |
| `rate_per_conn` | integer | no | `0` | Messages per second per connection. `0` is unthrottled. In an action script it paces every individual send. |
| `payload` | string | no | see below | Text payload for this phase. Supports `{{timestamp}}`, `{{worker_id}}` and `{{seq}}`. |
| `think_time` | duration | no | `0` | Pause after each `Send` action, or after each message when the phase has no actions. See [section 4](#4-how-a-phase-runs). |
| `actions` | list of actions | no | `[]` | Script each connection runs. Without actions the phase runs the `--mode` loop. |

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
| `Send` | `payload` (string, optional), `count` (integer, optional) | `count: 1` | Sends `count` messages back to back, each one paced by `rate_per_conn` when it is set. In echo mode each message waits for its reply and records an RTT sample. After the whole burst the worker sleeps `think_time`. `count` must be greater than zero. |
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

1. wsblast prints a banner on stderr, for example `▶ Scenario 'Chat session' phase 1/2: 'warmup' (5 connections for 3s)`.
2. It starts `connections` workers, `ramp_rate` per second.
3. The phase ends when its `duration` has elapsed (counted from the first worker, ramp included), when the `--requests` cap is reached, or on Ctrl+C. Every worker sends a Close frame and stops.
4. wsblast waits 500 ms and starts the next phase with new connections.

A phase always lasts its full `duration`, even if every connection has finished its script with a `Close` action or has given up with `--no-reconnect`.

### 4.2 Phases without actions

The phase runs the normal session loop for `--mode` (echo by default) with the phase's `connections`, `ramp_rate`, `rate_per_conn` and `payload`. `think_time` is applied:

| Mode | When `think_time` applies |
| :--- | :--- |
| `echo` | After each reply, before the next request. |
| `stream` | After each send. |
| `listen`, `graphql-ws` | Never. |

### 4.3 Phases with actions

Each connection runs the action list from the top, and starts again from the first action after the last one. It keeps looping until the phase ends or a `Close` action runs.

- Before every action and before every message of a `Send` burst, the worker checks for cancellation and for the `--requests` cap.
- If `rate_per_conn` is set, every message waits for the next tick of a per-connection interval of `1 / rate_per_conn` seconds. Missed ticks are skipped, not sent in a burst.
- In echo mode, a message that gets no reply within `--message-timeout` records a `timeout` error and drops the connection. Unless `--no-reconnect` is set, the worker reconnects with backoff (100 ms doubling to 5 s) and the script starts again from the first action on the new connection. A server Close frame has the same effect, without an error.
- In `stream` and `listen` mode a script sends without waiting for replies and does not read incoming frames, so the report shows 0 messages received. Use echo mode when the script needs replies.
- `--ping-interval` keepalives are not sent while a script runs. Add a `Ping` action instead.

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
| `--ramp-rate` | Ignored. Each phase's `ramp_rate` is used. |
| `--tui` | Rejected at startup (exit code 1). Use the progress bar or `--no-progress`. |
| `--mode` | Applies to every phase. |
| `-n, --requests` | One cap for the whole scenario. Once reached, the remaining phases are cancelled as soon as they start. |
| `--binary` | Changes only the default payload (see [section 2](#2-phase-fields)). |
| `--connect-timeout`, `--message-timeout`, `--ping-interval`, `-H`, `--subprotocol`, `--no-reconnect`, authentication flags | Apply to every phase. |
| SLO flags, `--format`, `-o`, `--no-progress` | Apply to the merged report. |

## 7. The report

All phases are merged into one report, and SLOs are evaluated once on the merged metrics. A failing phase therefore cannot be told apart from the others in the report; run it alone with `--phase` to gate it separately.

| Report field | Value for a scenario |
| :--- | :--- |
| `connections` | Sum of `connections` over the executed phases. |
| `duration_secs`, `metrics.elapsed` | Sum of each phase's elapsed time. The 500 ms pauses are not included. |
| `target_url` | Target of the first executed phase. |
| `phase_targets` | JSON only: one `{"phase", "target_url"}` entry per executed phase, in run order. The text and Markdown reports show the same list in place of the single target. |
| `metrics.total_connections_attempted` | Same as `connections`. |
| Latency histograms | One distribution over the samples of every phase. |

## 8. Stopping a scenario

Ctrl+C or SIGTERM stops the current phase and skips all remaining phases. wsblast then prints the merged report for what ran and exits with the normal exit code (0, or 2 if an SLO failed). A local run of example B below, interrupted after 2 s, reported `duration_secs` of 2.0, 10 connections (only the first phase) and exit code 0.

## 9. Validation

The whole file is validated before any connection is opened, including phases that `--phase` will not run. All of these fail with exit code 1:

| Problem | Message (after `Error: Configuration error: Invalid scenario:`) |
| :--- | :--- |
| `version` is not `"1.0"` | `unsupported version '2.0', expected '1.0'` |
| Empty `metadata.name` | `metadata.name must not be empty` |
| No phases | `scenario must define at least one phase` |
| Empty or duplicate phase name | `phase name must not be empty`, `duplicate phase name 'warmup'` |
| `connections: 0` | `phase 'warmup': connections must be greater than zero` |
| Zero `duration` | `phase 'warmup': duration must be greater than zero` |
| Unparsable duration | `phase 'warmup': Configuration error: Invalid duration 'abc': expected a non-negative number with a unit of ...` |
| Missing required field | ``load.yaml: phases[0]: missing field `duration` at line 6 column 5`` |
| Unknown or misspelled key | ``load.yaml: phases[0]: unknown field `conections`, expected one of `name`, `target`, ... at line 10 column 5`` |
| Unknown action type | ``load.yaml: phases[0].actions[0].type: unknown variant `send`, expected one of `Send`, `Wait`, `Close`, `Ping` `` |
| `count: 0` | `phase 'warmup': send action count must be greater than zero` |
| Zero `Wait` or `Ping` duration | `phase 'warmup': wait action duration must be greater than zero` |
| Forbidden or reserved close code | `close code 2000 cannot be sent; use 1000-1003, 1007-1014 or 3000-4999` |
| `--phase` names a missing phase | `phase 'nope' not found; available phases: soak, spike` |
| A phase that will run has no target from the command line, the phase or the scenario | ``phase 'spike': no target URL; set `target` on this phase or at the top level of the scenario, or pass a URL on the command line`` |

An invalid URL in any `target` fails with `Invalid target URL` or `Unsupported WebSocket scheme`.

## 10. Examples

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

# Only the spike phase, gated on its own
cargo run --release -- --scenario soak.yaml --phase spike \
  --no-progress --max-p99 50ms --max-error-rate 0.01

# Same file against another server: the command-line URL overrides both phase targets
cargo run --release -- --scenario soak.yaml --url wss://staging.example.com/ws
```

What happens:

- `soak`: the script has no `Close`, so each connection repeats it for the whole phase: 10 messages paced at 50 per second (about 200 ms), then a 100 ms pause.
- `spike`: no actions, so it runs the echo loop at 100 messages per second per connection, all 40 connections at once.

Local runs exited with 0. Both phases together reported 50 connections, about 13,800 messages (one run: 13,837 sent, 13,836 received; the difference is the request in flight when the phase ended), 0 errors and `duration_secs` of 9.0. `--phase spike` with the two SLO flags reported 40 connections, 12,000 messages, a p99 of 370 µs, both checks passed, and `duration_secs` of 3.0. Exact counts vary from run to run.
