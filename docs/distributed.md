# Distributed load generation

One machine runs out of CPU, file descriptors or ephemeral ports long before a large WebSocket service does. A distributed run spreads one test over several load machines: each runs `wsblast agent`, and a coordinator (`wsblast URL --agents ...`) splits the test between them, starts them together and merges their results into one report. The coordinator generates no load itself.

The merged report is the report one machine with every connection would have produced: counters are added, and the latency histograms themselves are merged, so p99 is the p99 of every sample, not an average of the agents' p99s. [METHODOLOGY.md, section 12](../METHODOLOGY.md#12-distributed-runs), defines every number of a distributed run.

It is part of the default build (Cargo feature `distributed`). The same agent and coordinator are available to Rust code as [`wsblast::distributed`](https://docs.rs/wsblast/latest/wsblast/distributed/index.html).

Contents:

1. [Running on N machines](#1-running-on-n-machines)
2. [Flags](#2-flags)
3. [How the load is split](#3-how-the-load-is-split)
4. [Architecture and protocol](#4-architecture-and-protocol)
5. [Security model](#5-security-model)
6. [Failures and exit codes](#6-failures-and-exit-codes)
7. [Limitations](#7-limitations)
8. [Using it from Rust](#8-using-it-from-rust)

## 1. Running on N machines

1. Pick a token of at least 16 characters and give it to every machine, for example in the environment:

   ```sh
   export WSBLAST_AGENT_TOKEN="$(openssl rand -hex 32)"
   ```

2. Install the same wsblast version on every load machine and start an agent on each:

   ```sh
   wsblast agent --listen 0.0.0.0:7700
   ```

   The agent prints `wsblast agent listening on ws://0.0.0.0:7700 (protocol 1, one test at a time)` and then one line per test it runs, naming the coordinator, the target and the connection count. Raise the open-file limit on the agent machines (`ulimit -n 65535`) as you would for a single-machine run.

3. On the coordinator, run the test as usual and add `--agents`:

   ```sh
   wsblast wss://api.example.com/stream -c 30000 -d 5m --rate 2 \
     --agents 10.0.0.11:7700,10.0.0.12:7700,10.0.0.13:7700 \
     --max-p99 200ms --format json -o report.json
   ```

   Each agent opens 10,000 of the 30,000 connections. The coordinator prints one progress bar for the whole run (summed from the agents' live counters), then the merged report, and evaluates the SLOs on the merged metrics with the usual exit codes ([section 6](#6-failures-and-exit-codes)).

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

Every other flag of a normal run keeps its meaning: target, mode, rate, payload, headers, subprotocol, timeouts, pings, `--match`, `--no-reconnect`, GraphQL options, `--auth-*`, SLOs, `--format` and `-o`. These are not supported with `--agents` in this version and are rejected with exit code 1: `--scenario`, `--capture`, `--tui`, `--auth-token-refresh`, `--prometheus-listen` and `--otel-endpoint`.

## 3. How the load is split

With N agents, in the order of `--agents`:

| Setting | Each agent gets |
| :--- | :--- |
| `-c` | The connections divided as evenly as possible; the remainder goes to the first agents, one each. `-c 10` over 3 agents is 4, 3, 3. |
| `-n` | Divided the same way, so the agents together send exactly `-n` messages, up to the usual overshoot of about one message per connection ([METHODOLOGY.md, section 10](../METHODOLOGY.md#10-known-limitations)). |
| `--ramp-rate` | Divided the same way, so every agent ramps its connections in about the same time as one machine would ramp all of them. |
| `--stages` | Every agent follows the whole profile and opens its share of the target at every moment: agent `i` opens the part `i` of `target(t)` divided as above. The agents' counts add up to the requested target at every point of the profile, not only at the ends of the stages. |
| `--rate` | Unchanged. It is a rate per connection, so the total rate is the same as on one machine. |
| Everything else | Unchanged. |

When a share would be zero, the run is rejected before anything connects: more agents than connections (or than the highest stage target), than `-n`, or than a non-zero `--ramp-rate`.

## 4. Architecture and protocol

The coordinator opens one WebSocket control connection to each agent, sending the token as `Authorization: Bearer TOKEN` in the handshake. Every message is a JSON text frame with a `type` field:

| Message | Direction | Content |
| :--- | :--- | :--- |
| `hello` | both | `protocol` (the protocol version, currently 1) and `version` (the wsblast version). The coordinator sends first and the agent answers. |
| `prepare` | coordinator to agent | `config`: the agent's share of the test, and `start_timeout`: how long the agent waits for `start`. |
| `ready` | agent to coordinator | `connections`: the agent validated the configuration, resolved the target, built its TLS settings and waits. Nothing has connected to the target yet. |
| `start` | coordinator to agent | Start the run now. |
| `snapshot` | agent to coordinator | Every second while the run lasts: `elapsed` and `live`, the agent's live counters. Progress only; they are never merged into the report. |
| `stop` | coordinator to agent | End the run early. Before `start` it cancels the test. |
| `result` | agent to coordinator | The outcome of the run, the last message an agent sends for a test. |
| `error` | both | `message`: the test cannot go on. The sender closes the connection after it. |

A test goes through these steps:

1. **Connect.** The agent checks the token in constant time. A missing or wrong token gets `HTTP 401`. An agent already running a test answers `HTTP 409` and the coordinator reports it as busy.
2. **Hello.** Both sides compare protocol versions. A mismatch is an `error` naming both wsblast versions; the `hello` message keeps the same shape in every version, so this check always works.
3. **Prepare.** The coordinator sends each agent its share. The agent rebuilds a configuration from it with the same builder and validation as the command line and answers `ready`, or an `error` explaining what it rejected.
4. **Barrier.** The coordinator waits until every agent is ready, for at most `--agent-timeout` in total. If any agent is unreachable, rejects the token, is busy, speaks another protocol, rejects the configuration or is not ready in time, the coordinator tells the ready agents to stop, prints which agents failed and why, and exits with code 1. Nothing has run.
5. **Start.** The coordinator sends `start` to every agent at once, on concurrent writes, and prints a bound on the start skew: the slowest `hello` round trip.
6. **Run.** Each agent runs its share exactly as a single-machine run would, ends at its duration, its request limit or a `stop`, and sends one `result`.
7. **Merge.** The coordinator merges every result that arrived and prints the report.

The `config` of `prepare` is a dedicated wire structure, not a serialised `LoadTestConfig`: the target, the mode, the connections, ramp rate and request limit of this agent, the duration, the stages and the agent's share of them, the rate, the payload (text as is, binary as base64), the handshake headers (including resolved `--auth-*` credentials), the subprotocol, the timeouts, the ping interval, `--match`, the reconnect setting and the GraphQL settings. SLOs, the report format and file paths stay on the coordinator.

A `result` carries the agent's counters, its error breakdown, its interrupted and not-started slot counts, its handshake, RTT and response-time histograms as `[value, count]` pairs of every non-empty bucket, its time series, and the latency histogram of every time-series interval in the same form. A histogram rebuilt from these pairs has exactly the agent's buckets, so merging rebuilt histograms gives the same percentiles as merging the agents' own.

Example `prepare` and `result` messages, abbreviated:

```json
{"type":"prepare","start_timeout":{"secs":30,"nanos":0},"config":{"target":"wss://api.example.com/stream","mode":"echo","connections":4,"ramp_rate":0,"duration":{"secs":300,"nanos":0},"stages":[],"stage_share":null,"max_requests":null,"rate_per_conn":2,"payload":{"kind":"text","text":"{\"seq\":{{seq}}}"},"headers":[["authorization","Bearer ..."]],"reply_match":"json:/seq","reconnect":true,"graphql":null,"...":"..."}}
{"type":"result","result":{"elapsed":{"secs":300,"nanos":1200000},"started_at_unix_ms":1761400000123,"connections":4,"not_started":0,"totals":{"messages_sent":2400,"rtt":[[812,3],[815,9],"..."],"...":"..."},"time_series":{"...":"..."},"intervals":[{"index":0,"values":[[810,4],"..."]},"..."]}}
```

## 5. Security model

An agent opens connections to whatever target an authenticated coordinator names, with whatever headers it sends, so the token is what stands between the agent and anyone who can reach its port.

- **The token is required.** An agent does not start without one, and both sides reject tokens shorter than 16 characters. Use a long random value (`openssl rand -hex 32`) and keep it out of shell history, for example in `WSBLAST_AGENT_TOKEN`. The agent compares tokens in constant time and logs every rejected connection to stderr with the peer address.
- **The control channel carries secrets.** The token travels in the handshake of every control connection, and the test's handshake headers travel in `prepare`, including credentials from `--auth-token`, `--auth-apikey`, `--auth-token-cmd` or `-H "Authorization: ..."`. Over plain `ws://` anyone on the path can read them. Use one of:
  - TLS: start the agents with `--tls-cert` and `--tls-key` and list them as `wss://HOST:PORT`; give the coordinator `--agent-ca` when the certificate is not signed by a public CA;
  - a private network (VPC, VPN, WireGuard) that only the coordinator and the agents can reach;
  - an SSH tunnel per agent: `ssh -N -L 7701:127.0.0.1:7700 agent1` on the coordinator, the agent started with `--listen 127.0.0.1:7700`, and `--agents 127.0.0.1:7701`.
- **Bind narrowly.** Listen on the address the coordinator uses, not on every interface, and firewall the port to the coordinator.
- **One coordinator at a time.** An agent runs one test at a time and turns other coordinators away with `HTTP 409` until it is done, so a second coordinator cannot hijack or disturb a running test.
- The coordinator does not authenticate agents beyond TLS: with `ws://`, anyone who can redirect traffic to an agent address can receive the token and the test's headers.

## 6. Failures and exit codes

| Situation | What happens | Exit code |
| :--- | :--- | :--- |
| An agent is unreachable, rejects the token (401), is busy (409), speaks another protocol, rejects the configuration, or is not ready within `--agent-timeout` | The ready agents are told to stop, nothing runs, and the error names each failing agent and the reason. | 1 |
| Ctrl+C or SIGTERM on the coordinator before the start | Nothing runs. | 1 |
| Ctrl+C or SIGTERM on the coordinator after the start | `stop` is sent to every agent; each stops its run and sends its result, which is merged and reported as for a run that ended at its duration. | by the usual rules |
| An agent disconnects, sends an `error`, or sends no result within `--agent-timeout` after the end of the run (or after a `stop`) | The run still reports the agents that finished. A warning names each lost agent and the reason, the report shows `Agents: N (lost: ...)`, and the JSON report lists it in `metrics.distributed.lost_agents`. | 1, or 2 if an SLO failed |
| Every agent is lost | There is nothing to report; the error names every agent and its reason. | 1 |
| A coordinator disconnects from an agent | The agent stops that run at once and is free for the next coordinator. | (agent keeps running) |

A lost agent's work is never re-run on another agent, so nothing is counted twice; its share of connections and messages is simply missing from the report, which is why a run with a lost agent exits with 1. Other exit codes follow [METHODOLOGY.md, section 9](../METHODOLOGY.md#9-slo-evaluation-and-exit-codes).

## 7. Limitations

- Not supported with `--agents` in this version: scenarios (`--scenario`), `--capture`, the TUI, `--auth-token-refresh`, and live export (`--prometheus-listen`, `--otel-*`). Each is rejected with exit code 1.
- Run the same wsblast version everywhere. A different protocol version is rejected; the same protocol from another release works, but the agents then measure with their own release's rules.
- `--auth-token-cmd` runs once, on the coordinator, and the resulting header is sent to every agent.
- Agents start within the control channel's latency of each other, not at the same instant, and their time series are aligned on their own starts ([METHODOLOGY.md, section 12.3](../METHODOLOGY.md#123-time-across-machines)). Keep the coordinator close to the agents when per-second detail matters.
- The coordinator does not check that the agents are not saturated. Watch their CPU as you would for a single machine.
- The agents' wall clocks are not used for any measurement; `started_at_unix_ms` in the library's per-agent outcome is informational.

## 8. Using it from Rust

`wsblast::distributed` has the same agent and coordinator as the command line. `AgentConfig::builder()` and `AgentServer::bind` start an agent in-process (port 0 works); `Coordinator::new(config, agents, token)` takes a `LoadTestConfig`, and `run()` returns a `DistributedOutcome` with the merged `AggregatedMetrics` and, for each agent, its own metrics or the reason it was lost. The crate's `tests/distributed.rs` runs agents and a coordinator against the mock server in one process.
