# Distributed load generation

One machine runs out of CPU, file descriptors or ephemeral ports long before a large WebSocket service does. A distributed run spreads one test over several load machines: each runs `wsblast agent`, and a coordinator (`wsblast URL --agents ...`) splits the test between them, starts them together and merges their results into one report. The coordinator generates no load itself.

The merged report is the report one machine with every connection would have produced: counters are added, and the latency histograms themselves are merged, so p99 is the p99 of every sample, not an average of the agents' p99s. [METHODOLOGY.md, section 12](../METHODOLOGY.md#12-distributed-runs), defines every number of a distributed run.

It is part of the default build (Cargo feature `distributed`). The same agent and coordinator are available to Rust code as [`wsblast::distributed`](https://docs.rs/wsblast/latest/wsblast/distributed/index.html).

Contents:

1. [Running on N machines](#1-running-on-n-machines)
2. [Flags](#2-flags)
3. [How the load is split](#3-how-the-load-is-split)
4. [Scenarios, capture, refreshed tokens and live export](#4-scenarios-capture-refreshed-tokens-and-live-export)
5. [Chaos, generator saturation and the dashboard](#5-chaos-generator-saturation-and-the-dashboard)
6. [Architecture and protocol](#6-architecture-and-protocol)
7. [Security model](#7-security-model)
8. [Failures and exit codes](#8-failures-and-exit-codes)
9. [Limitations](#9-limitations)
10. [Using it from Rust](#10-using-it-from-rust)

## 1. Running on N machines

1. Pick a token of at least 16 characters and give it to every machine, for example in the environment:

   ```sh
   export WSBLAST_AGENT_TOKEN="$(openssl rand -hex 32)"
   ```

2. Install the same wsblast version on every load machine and start an agent on each:

   ```sh
   wsblast agent --listen 0.0.0.0:7700
   ```

   The agent prints `wsblast agent listening on ws://0.0.0.0:7700 (protocol 3, one test at a time)` and then one line per test it runs, naming the coordinator, the target and the connection count. Raise the open-file limit on the agent machines (`ulimit -n 65535`) as you would for a single-machine run.

3. On the coordinator, run the test as usual and add `--agents`:

   ```sh
   wsblast wss://api.example.com/stream -c 30000 -d 5m --rate 2 \
     --agents 10.0.0.11:7700,10.0.0.12:7700,10.0.0.13:7700 \
     --max-p99 200ms --format json -o report.json
   ```

   Each agent opens 10,000 of the 30,000 connections. The coordinator prints one progress bar for the whole run (summed from the agents' live counters), then the merged report, and evaluates the SLOs on the merged metrics with the usual exit codes ([section 7](#8-failures-and-exit-codes)).

An agent serves one test after another for as long as it runs; stop it with Ctrl+C or SIGTERM. A test in progress is then stopped and still sends its result.

## 2. Flags

`wsblast agent`:

| Flag | Default | Effect |
| :--- | :--- | :--- |
| `--listen ADDR` | required | Address to accept coordinators on. Port 0 picks a free port; the startup line shows it. |
| `--token TOKEN` | `$WSBLAST_AGENT_TOKEN` | Token a coordinator must present. Required, at least 16 characters, no whitespace. The agent refuses to start without one. |
| `--tls-cert PEM`, `--tls-key PEM` | off | Serve the control channel over TLS (`wss://`) with this PEM certificate chain and private key (PKCS#8, PKCS#1 or SEC1). |

The coordinator is the normal command plus:

| Flag | Default | Effect |
| :--- | :--- | :--- |
| `--agents LIST` | off | Comma-separated agents. `HOST:PORT` connects with plain `ws://`; write `wss://HOST:PORT` for an agent started with `--tls-cert`. An agent cannot be listed twice. |
| `--agent-token TOKEN` | `$WSBLAST_AGENT_TOKEN` | The agents' token. |
| `--agent-timeout DURATION` | `30s` | Time the agents get to connect, authenticate, validate the test and become ready (the start barrier), and time each agent gets after the end of the run, or after a stop, to send its result. |
| `--agent-ca PEM` | off | CA certificates to trust for `wss://` agents, in addition to the platform's roots (which `SSL_CERT_FILE` and `SSL_CERT_DIR` also override). Use it for a private CA or a self-signed agent certificate. |

Every other flag of a normal run keeps its meaning: target, mode, rate, payload, headers, subprotocol, timeouts, pings, `--match`, `--no-reconnect`, GraphQL options, `--auth-*` including `--auth-token-refresh`, `--scenario` and `--phase`, `--capture` and `--capture-limit`, `--prometheus-listen`, `--otel-*`, `--chaos-*`, SLOs including `--max-generator-lag`, `--tui`, `--format` and `-o`. [Section 4](#4-scenarios-capture-refreshed-tokens-and-live-export) describes how the scenario, capture, token refresh and export flags work across machines, and [section 5](#5-chaos-generator-saturation-and-the-dashboard) how chaos, the generator lag and the dashboard do. Only `--auth-jwt-per-connection` is rejected with `--agents` (exit code 1): a token signed for every connection would need the signing key on every agent, and the key stays on the coordinator. Without it, the shared token is re-signed before it expires and pushed to the agents like a refreshed token ([section 4.3](#43-refreshed-tokens)).

## 3. How the load is split

With N agents, in the order of `--agents`:

| Setting | Each agent gets |
| :--- | :--- |
| `-c` | The connections divided as evenly as possible; the remainder goes to the first agents, one each. `-c 10` over 3 agents is 4, 3, 3. |
| `-n` | Divided the same way, so the agents together send exactly `-n` messages, up to the usual overshoot of about one message per connection ([METHODOLOGY.md, section 10](../METHODOLOGY.md#10-known-limitations)). |
| `--ramp-rate` | Divided the same way, so every agent ramps its connections in about the same time as one machine would ramp all of them. |
| `--stages` | Every agent follows the whole profile and opens its share of the target at every moment: agent `i` opens the part `i` of `target(t)` divided as above. The agents' counts add up to the requested target at every point of the profile, not only at the ends of the stages. |
| `--arrival-rate` | Divided equally, as a number of sessions per second, so the agents together start sessions at the requested rate. `--session-duration` is unchanged, and `--max-connections` is divided like `-c`. The merged `metrics.arrivals` adds up the agents' counts, so its `peak_open` is the sum of each agent's peak. |
| `--rate` | Unchanged. It is a rate per connection, so the total rate is the same as on one machine. |
| Everything else | Unchanged. |

When a share would be zero, the run is rejected before anything connects: more agents than connections (or than the highest stage target), than `-n`, than a non-zero `--ramp-rate`, or than `--max-connections`.

## 4. Scenarios, capture, refreshed tokens and live export

### 4.1 Scenarios

`--scenario FILE` runs the scenario's phases one after another, as on one machine, and each phase is a distributed run of its own:

- The coordinator compiles the scenario and resolves every phase's target. Each phase is split between the agents exactly like a single run ([section 3](#3-how-the-load-is-split)): its connections, `-n` and ramp rate are divided, a staged phase's profile is shared at every moment, and the phase's actions, think time and payload go to every agent unchanged. A phase that cannot be split (fewer connections, or a lower highest stage target, than agents) is rejected before anything connects, naming the phase.
- Every phase has its own start barrier: the agents prepare it, report ready, and start together on one `start` message. Phases are 500 ms apart, as on one machine.
- Each phase's results are merged exactly, histograms included. The phase's own SLOs (`slo:` on the phase) are checked on its merged metrics, and the scenario's SLOs (`slo:` at the top level and the SLO flags) on the metrics merged over every phase, with the usual exit codes. The phases' time series are laid end to end, each counted from the coordinator's start of the phase.
- Ctrl+C or SIGTERM stops the current phase on every agent, which still send their results, and skips the remaining phases. A stop during a phase's barrier skips that phase too.
- One control connection carries the whole scenario, so an agent stays busy (and turns other coordinators away) from the first phase to the last.
- The agents' live counters count from the start of the scenario, across phases, as one machine's do.

An agent lost in a phase takes no part in the later ones, and its shares of them are missing from the report; the other agents keep their own shares, so nothing is counted twice. An agent that fails the barrier of a later phase (it rejects the phase or is not ready within `--agent-timeout`) is lost in the same way and the phase runs on the others. The report lists every agent lost in any phase.

### 4.2 Capture

With `--capture FILE`, every agent records its frames exactly as a single run does ([METHODOLOGY.md, section 11](../METHODOLOGY.md#11-frame-capture)): the same bounded queue of 65,536 records, filled with a non-blocking `try_send`, so a record that cannot be queued is dropped and counted and the workers never wait. Instead of a file, a thread on the agent formats the records and sends them in batches of up to 1,024 to the coordinator over the control channel. When the control channel or the coordinator's disk is slower than the agents produce records, that thread waits, the agent's queue fills and the agent drops records; the load never slows down.

The coordinator writes one file in the same JSON Lines format. The header line also lists the agents, and every record has an `agent` field, the agent's position in `--agents` from 0:

```json
{"wsblast_capture":1,"target":"ws://10.0.0.20:9001/","started":"2026-10-25T09:30:00.123Z","agents":["10.0.0.11:7700","10.0.0.12:7700"]}
{"t_us":1532,"agent":0,"worker":0,"dir":"recv","kind":"open","len":0}
{"t_us":1544,"agent":1,"worker":0,"dir":"recv","kind":"open","len":0}
```

- `worker` is the connection slot on that agent; each agent numbers its slots from 0.
- `t_us` counts from the coordinator's start of the run (of the first phase for a scenario): each agent's records are timed on the agent from its own start of the phase and shifted by when the coordinator started that phase. Records of different agents are therefore aligned to within the start skew, like their time series ([METHODOLOGY.md, section 12.3](../METHODOLOGY.md#123-time-across-machines)). Lines are written in the order they arrive, so the file is ordered per agent, not globally.
- `--capture-limit` applies to the whole run. Every agent may queue up to what is left of the limit when a phase starts, and the coordinator writes records up to the limit in the order they arrive and counts the rest as dropped. So the file holds at most the limit, and each agent sends at most that many records per phase.
- The report's `Capture: N frames written, M dropped` and `metrics.capture` cover every agent: `frames_written` is what the coordinator wrote, `frames_dropped` adds the records the agents dropped, the ones over the limit and any lost to a write error. The records of an agent lost before its result are not counted as dropped.

The format version stays 1: the `agent` field and the header's `agents` are additions. `wsblast mock --replay` reads such a file and keeps every agent's connections apart ([docs/mock.md, section 5](mock.md#5-recording-and-replaying-a-real-server)).

### 4.3 Refreshed tokens

With `--auth-token-cmd` and `--auth-token-refresh`, the token command runs on the coordinator only, at the start and then on its refresh schedule. Each `prepare` carries the current header value, and every new value is sent to every running agent in a `token` message within about 100 ms of the refresh. An agent keeps the value in a shared live header exactly as one machine does: every handshake from then on, first connection or reconnect, carries the new value, and established connections are not touched. A failed refresh keeps the previous value on the coordinator and therefore on every agent, with the usual warning. Neither side ever prints the value, and it is not part of any error message.

Signed and fetched tokens travel the same way ([docs/ci.md, sections 2.2 and 2.3](ci.md#22-jwts-signed-by-wsblast)):

- `--auth-type jwt` signs the shared token on the coordinator, which alone reads the key, and signs a new one before the current one expires; each new token reaches the agents in a `token` message. The agents' handshakes therefore carry tokens the server sees change over a long run, all signed by the coordinator. `--auth-jwt-per-connection` is rejected with `--agents`, since a token per connection attempt would need the key on every agent.
- `--auth-type oauth2` requests the access token on the coordinator only, and requests a new one before it expires: the token endpoint sees one client, not one per agent, and every refreshed access token reaches the agents in a `token` message.

### 4.4 Live export

`--prometheus-listen` and `--otel-endpoint` run on the coordinator and export the cluster's numbers, with the names and meanings of [METHODOLOGY.md, section 6.4](../METHODOLOGY.md#64-live-export):

- The counters and `wsblast_active_connections` are the sums of every agent's latest snapshot. Agents send one every second and once more at the end of each phase, so the exported values trail the agents by up to one second more than on a single machine. The counters of a lost agent stay at its last snapshot and its open connections count as 0.
- The latency is that of the last completed time-series interval merged from every agent's interval histogram: each snapshot carries the agent's two most recently completed intervals, and the coordinator exports an interval one second after it ended on its own clock, once every agent has had a tick to send it. So the latency lags the run by up to one interval plus one second; when a phase ends, the last interval that ended before it is exported from the agents' results.
- `wsblast_run_elapsed_seconds` runs on the coordinator's clock from the first phase's start.

The coordinator keeps these cluster totals in a live-metrics registry of its own whether or not an exporter runs, and the dashboard reads them ([section 5.3](#53-the-dashboard)).

## 5. Chaos, generator saturation and the dashboard

### 5.1 Chaos injection

The `--chaos-*` options, and a scenario phase's `chaos:` mapping, work with `--agents` as on one machine ([METHODOLOGY.md, section 13](../METHODOLOGY.md#13-chaos-injection)): every agent injects the faults into its own connections, with the same percentages, delay and close mode.

- **Seeds.** The run has one seed: `--chaos-seed`, the phase's `seed`, or one the coordinator picks. Each agent draws its decisions from a seed of its own, derived from the run's seed and the agent's position in `--agents` with SplitMix64. Every agent numbers its connection slots from 0, so with the run's seed itself the agents would all make the same decisions on the same slots; with their own seeds they do not, and a run with the same seed and the same `--agents` order repeats every agent's decisions. Moving an agent to another position in `--agents` gives it another seed.
- **Scenarios.** The coordinator resolves each phase's chaos (the phase's mapping, or the command line's options when it has none, and the seed) and sends every agent its share of it with the agent's seed, so the agents do not fall back on settings of their own.
- **Counts.** Each agent's result carries its counts of delayed, dropped and corrupted sends and of closes. The coordinator adds them up, so `metrics.chaos` holds the counts of every agent and the run's seed, the one to give `--chaos-seed` to repeat the run; a scenario reports the seed of its first phase that injected faults, as on one machine. Each agent's own counts and seed are in its [`AgentOutcome`](#10-using-it-from-rust) when the coordinator runs from Rust.
- The counts of a whole run repeat when the number of sends does, as on one machine: with `-n` and one connection per agent they repeat exactly.

### 5.2 Generator saturation

Every agent watches its own load generator as a single run does ([METHODOLOGY.md, section 14](../METHODOLOGY.md#14-load-generator-saturation)): its lag probe runs for the whole of each run or phase, and it checks its open file limit against its share of the connections. Its result carries the lag samples and the limit; lag does not add up across machines, so the coordinator keeps each agent's apart:

- The report lists every agent's lag and limit in `metrics.distributed.generators`.
- `metrics.generator_lag` is the lag of the agent with the highest p99 lag, named in `metrics.distributed.generator_lag_agent`, and it is flagged `saturated` when any agent's generator was saturated. `metrics.open_file_limit` is the limit of the agent with the fewest descriptors to spare, named in `metrics.distributed.open_file_limit_agent`.
- `--max-generator-lag D` compares that highest p99 lag with `D`: the gate fails if any agent's generator was saturated past the limit, and the run exits with 2.
- The text, Markdown and HTML reports print the `Generator:` line with the agent whose lag and limit it shows and the agents that were saturated or short of descriptors, for example `lag p99 31.20 ms, max 64.00 ms over 150 wake-ups on agent 10.0.0.12:7700, the highest of 3 agents (threshold 20.00 ms; SATURATED on 10.0.0.12:7700: latency includes client-side delay)`. After the report the coordinator warns once for every saturated agent.
- Each agent checks its lag against its own platform's threshold, so a cluster that mixes Windows and Linux agents flags each of them correctly.

### 5.3 The dashboard

`--tui` draws the same live dashboard on the coordinator as on one machine ([METHODOLOGY.md, section 6](../METHODOLOGY.md#6-counters-rates-and-formulas)), for a single run and for every phase of a scenario, from what the coordinator knows:

- The counters, the open connections and the error and throughput windows are the cluster totals of [section 4.4](#44-live-export): the sum of every agent's latest snapshot, one second old at most.
- p50, p90 and p99 are those of the last completed interval merged from every agent's interval histogram, the value the exporters publish, so they trail the run by up to one interval plus one second.
- The progress bar, the stage and the target connection count follow the coordinator's clock from the start of each phase; the phase list shows each finished phase's SLO result on its merged metrics.
- The header shows the number of agents, and in red each agent lost so far.
- `q`, `Esc` and Ctrl+C stop the run exactly as Ctrl+C on a coordinator without the dashboard does: every agent is told to stop, each sends its result, and the merged report follows the dashboard with the usual exit code. The scenario phase lines and the stop message that the coordinator prints on stderr are left out while the dashboard is drawn.

## 6. Architecture and protocol

The coordinator opens one WebSocket control connection to each agent, sending the token as `Authorization: Bearer TOKEN` in the handshake. Every message is a JSON text frame with a `type` field:

| Message | Direction | Content |
| :--- | :--- | :--- |
| `hello` | both | `protocol` (the protocol version, currently 3) and `version` (the wsblast version). The coordinator sends first and the agent answers. |
| `prepare` | coordinator to agent | `config`: the agent's share of the test or of one scenario phase, and `start_timeout`: how long the agent waits for `start`. |
| `ready` | agent to coordinator | `connections`: the agent validated the configuration, resolved the target, built its TLS settings and waits. Nothing has connected to the target yet. |
| `start` | coordinator to agent | Start the run now. |
| `snapshot` | agent to coordinator | Every second while the run lasts, and once more just before `result`: `elapsed`, `live`, the agent's live counters since the start of the test, and `intervals`, the latency histograms of the phase's two most recently completed time-series intervals. Progress and live export only; they are never merged into the report. |
| `capture` | agent to coordinator | `records`: a batch of capture records, with `--capture` only ([section 4.2](#42-capture)). |
| `token` | coordinator to agent | `value`: the new value of the authentication header, with `--auth-token-refresh` only ([section 4.3](#43-refreshed-tokens)). |
| `stop` | coordinator to agent | End the run early. Before `start` it cancels the test. |
| `result` | agent to coordinator | The outcome of the run or phase. |
| `error` | both | `message`: the test cannot go on. The sender closes the connection after it. |

A test goes through these steps:

1. **Connect.** The agent checks the token in constant time. A missing or wrong token gets `HTTP 401`. An agent already running a test answers `HTTP 409` and the coordinator reports it as busy.
2. **Hello.** Both sides compare protocol versions. A mismatch is an `error` naming both wsblast versions; the `hello` message keeps the same shape in every version, so this check always works.
3. **Prepare.** The coordinator sends each agent its share. The agent rebuilds a configuration from it with the same builder and validation as the command line and answers `ready`, or an `error` explaining what it rejected.
4. **Barrier.** The coordinator waits until every agent is ready, for at most `--agent-timeout` in total. If any agent is unreachable, rejects the token, is busy, speaks another protocol, rejects the configuration or is not ready in time, the coordinator tells the ready agents to stop, prints which agents failed and why, and exits with code 1. Nothing has run.
5. **Start.** The coordinator sends `start` to every agent at once, on concurrent writes, and prints a bound on the start skew: the slowest `hello` round trip.
6. **Run.** Each agent runs its share exactly as a single-machine run would, ends at its duration, its request limit or a `stop`, sends the rest of its capture records and a last `snapshot`, and sends one `result`.
7. **Next phase.** For a scenario, the coordinator sends the next phase's `prepare` on the same connection, and steps 3 to 6 repeat. An agent waits for it for the phase's duration plus `start_timeout` plus 60 seconds.
8. **Merge.** The coordinator closes the control connections, merges every result that arrived and prints the report.

The `config` of `prepare` is a dedicated wire structure, not a serialised `LoadTestConfig`: the target, the mode, the connections, ramp rate and request limit of this agent, the duration, the stages and the agent's share of them, the agent's share of an arrival-rate model (`arrivals`, absent otherwise), the rate, the payload (text as is, binary as base64), the handshake headers (including resolved `--auth-*` credentials), the subprotocol, the timeouts, the ping interval, `--match`, the reconnect setting and the GraphQL settings; and, when they apply, `phase` (the scenario phase's name, think time and actions), `capture` (how many records the agent may still queue), `live_auth` (the refreshed header's name and current value) and `chaos` (the faults to inject, with the agent's seed). SLOs, the report format and file paths stay on the coordinator.

Version 2 of the protocol added scenario phases, the `capture` and `token` messages and the snapshot's `intervals`. Version 3 added `chaos` to the configuration, and the agent's counts of injected faults, its generator lag and its open file limit to its result. Builds speaking different versions reject each other at the hello, with an error naming both wsblast versions: an agent of an earlier release cannot be given chaos settings it would ignore.

A `result` carries the agent's counters, its error breakdown, its interrupted and not-started slot counts, its handshake, RTT and response-time histograms as `[value, count]` pairs of every non-empty bucket, its time series, the latency histogram of every time-series interval in the same form, its generator lag histogram in the same form with its interval counts and threshold, its open file limit, and, when it injected faults, its chaos counts. A histogram rebuilt from these pairs has exactly the agent's buckets, so merging rebuilt histograms gives the same percentiles as merging the agents' own.

Example `prepare` and `result` messages, abbreviated:

```json
{"type":"prepare","start_timeout":{"secs":30,"nanos":0},"config":{"target":"wss://api.example.com/stream","mode":"echo","connections":4,"ramp_rate":0,"duration":{"secs":300,"nanos":0},"stages":[],"stage_share":null,"max_requests":null,"rate_per_conn":2,"payload":{"kind":"text","text":"{\"seq\":{{seq}}}"},"headers":[["authorization","Bearer ..."]],"reply_match":"json:/seq","reconnect":true,"graphql":null,"...":"..."}}
{"type":"result","result":{"elapsed":{"secs":300,"nanos":1200000},"started_at_unix_ms":1761400000123,"connections":4,"not_started":0,"totals":{"messages_sent":2400,"rtt":[[812,3],[815,9],"..."],"...":"..."},"time_series":{"...":"..."},"intervals":[{"index":0,"values":[[810,4],"..."]},"..."]}}
```

## 7. Security model

An agent opens connections to whatever target an authenticated coordinator names, with whatever headers it sends, so the token is what stands between the agent and anyone who can reach its port.

- **The token is required.** An agent does not start without one, and both sides reject tokens shorter than 16 characters. Use a long random value (`openssl rand -hex 32`) and keep it out of shell history, for example in `WSBLAST_AGENT_TOKEN`. The agent compares tokens in constant time and logs every rejected connection to stderr with the peer address.
- **The control channel carries secrets.** The token travels in the handshake of every control connection, and the test's handshake headers travel in `prepare`, including credentials from `--auth-token`, `--auth-apikey`, `--auth-token-cmd` or `-H "Authorization: ..."`, as do refreshed tokens in `token` messages. Capture records carry the frames' payloads. Over plain `ws://` anyone on the path can read them. Use one of:
  - TLS: start the agents with `--tls-cert` and `--tls-key` and list them as `wss://HOST:PORT`; give the coordinator `--agent-ca` when the certificate is not signed by a public CA;
  - a private network (VPC, VPN, WireGuard) that only the coordinator and the agents can reach;
  - an SSH tunnel per agent: `ssh -N -L 7701:127.0.0.1:7700 agent1` on the coordinator, the agent started with `--listen 127.0.0.1:7700`, and `--agents 127.0.0.1:7701`.
- **Bind narrowly.** Listen on the address the coordinator uses, not on every interface, and firewall the port to the coordinator.
- **One coordinator at a time.** An agent runs one test at a time and turns other coordinators away with `HTTP 409` until it is done, so a second coordinator cannot hijack or disturb a running test.
- The coordinator does not authenticate agents beyond TLS: with `ws://`, anyone who can redirect traffic to an agent address can receive the token and the test's headers.

## 8. Failures and exit codes

| Situation | What happens | Exit code |
| :--- | :--- | :--- |
| An agent is unreachable, rejects the token (401), is busy (409), speaks another protocol, rejects the configuration, or is not ready within `--agent-timeout` | The ready agents are told to stop, nothing runs, and the error names each failing agent and the reason. | 1 |
| Ctrl+C or SIGTERM on the coordinator before the start | Nothing runs. | 1 |
| Ctrl+C or SIGTERM on the coordinator after the start | `stop` is sent to every agent; each stops its run and sends its result, which is merged and reported as for a run that ended at its duration. | by the usual rules |
| An agent disconnects, sends an `error`, or sends no result within `--agent-timeout` after the end of the run (or after a `stop`) | The run still reports the agents that finished. A warning names each lost agent and the reason, the report shows `Agents: N (lost: ...)`, and the JSON report lists it in `metrics.distributed.lost_agents`. | 1, or 2 if an SLO failed |
| Every agent is lost | There is nothing to report; the error names every agent and its reason. | 1 |
| In a scenario, an agent fails the barrier of a later phase, or is lost during a phase | The agent is lost: the phase runs on the others, the agent takes no part in later phases, and it is reported as above with the phase in its reason. If every agent is lost, the remaining phases are skipped and the phases that ran are reported. | 1, or 2 if an SLO failed |
| A coordinator disconnects from an agent | The agent stops that run at once and is free for the next coordinator. | (agent keeps running) |

A lost agent's work is never re-run on another agent, so nothing is counted twice; its share of connections and messages is simply missing from the report, which is why a run with a lost agent exits with 1. Other exit codes follow [METHODOLOGY.md, section 9](../METHODOLOGY.md#9-slo-evaluation-and-exit-codes).

## 9. Limitations

- The dashboard's latency trails a single machine's by up to one second, like the live export.
- Live export trails a single machine's by up to one second ([section 4.4](#44-live-export)).
- Run the same wsblast version everywhere. A different protocol version is rejected; the same protocol from another release works, but the agents then measure with their own release's rules.
- `--auth-token-cmd` runs on the coordinator only, once at the start and again on every `--auth-token-refresh`, and the resulting header is sent to every agent.
- Agents start within the control channel's latency of each other, not at the same instant, and their time series are aligned on their own starts ([METHODOLOGY.md, section 12.3](../METHODOLOGY.md#123-time-across-machines)). Keep the coordinator close to the agents when per-second detail matters.
- The agents' wall clocks are not used for any measurement; `started_at_unix_ms` in the library's per-agent outcome is informational.

## 10. Using it from Rust

`wsblast::distributed` has the same agent and coordinator as the command line. `AgentConfig::builder()` and `AgentServer::bind` start an agent in-process (port 0 works); `Coordinator::new(config, agents, token)` takes a `LoadTestConfig`, and `run()` returns a `DistributedOutcome` with the merged `AggregatedMetrics` and, for each agent, its own metrics or the reason it was lost. A configuration built with `.capture(path)` captures across the agents. `Coordinator::scenario(scenario, base_config, phase, target, agents, token)` takes a `ScenarioConfig` like `ScenarioExecutor::new`, and `run_scenario()` returns a `DistributedScenarioOutcome`: the `ScenarioOutcome` a single machine would return (merged metrics, every phase with its merged metrics and SLO result, and the SLO gate of the run), each agent over the whole scenario, and each agent's part of every phase. `Coordinator::live_snapshot()` reads the cluster's live counters while a run is in progress. The crate's `tests/distributed.rs` runs agents and a coordinator against the mock server in one process.
