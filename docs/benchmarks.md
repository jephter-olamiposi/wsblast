# Benchmarks

This page compares wsblast with [k6](https://k6.io) and
[Artillery](https://www.artillery.io) on three questions that matter for
WebSocket load tests:

1. How many messages can one CPU core drive?
2. How much memory does each connection cost?
3. Does a server stall show up in the latency numbers?

Every number below comes from `scripts/compare/run.sh`, which you can run on
your own machine. The scripts for k6 and Artillery are in the same folder.
They are kept as short as each tool allows; if one of them does its tool an
injustice, please open an issue or a pull request.

## Setup

- wsblast 0.2.2, k6 v1.8.2, Artillery 2.0.34
- Linux 6.18, Intel Xeon at 2.10 GHz, 2 cores, 8 GB of memory
- The target is `wsblast mock`, an echo server, on core 1. Each tool runs on
  core 0 (`taskset`), so "one core" means one core.
- Payloads are short text frames (`hello`).

Results from a 2-core virtual machine are a lower bound. Run the script on
the hardware you care about.

## 1. Throughput on one core

Fifty connections, each sending its next message as soon as the previous
reply arrives, for 30 seconds.

| Tool | Replies per second | Tool CPU time | Replies per CPU second |
| :--- | ---: | ---: | ---: |
| wsblast | 30,720 | 12.9 s | 71,496 |
| k6 | 26,997 | 25.9 s | 31,235 |
| Artillery | 19,273 | 33.2 s | 17,400 |

The echo server, on its own core, is the limit in the first column: wsblast
used 43% of its core to reach it, k6 86%. The last column
is the comparison between the tools: replies divided by the CPU time the
tool used. It includes startup, which costs Artillery about 2.8 seconds of CPU
and the others under 0.2 seconds. Without it, Artillery's figure would be
about 19,000.

## 2. Memory per connection

Peak resident memory of the tool's processes with 500 and with 2,000
connections, each sending one message a second for 20 seconds. The last
column is the slope between the two runs, so fixed costs cancel out.

| Tool | Peak at 500 | Peak at 2,000 | Per additional connection |
| :--- | ---: | ---: | ---: |
| wsblast | 77 MB | 102 MB | 17 KB |
| k6 | 164 MB | 537 MB | 254 KB |
| Artillery | 326 MB | 342 MB | 11 KB |

In wsblast 0.2.2, the fixed cost includes 64 MiB of read buffers shared by
all connections. Later versions use 16 MiB (see
[METHODOLOGY.md](../METHODOLOGY.md), section 10). k6 runs a
JavaScript runtime for each virtual user, which is most of its cost per
connection. Artillery's Node.js process starts large and then grows slowly.

## 3. A server that stalls

The server stops replying for 1 second every 10 seconds. Ten connections send
20 messages a second each, for 30 seconds. About 10% of the requests are due
while the server is stalled, so a correct p99 is close to a second.

| Tool and metric | p50 | p99 | max |
| :--- | ---: | ---: | ---: |
| wsblast, response time (from schedule) | 2 ms | 888 ms | 991 ms |
| wsblast, round-trip time | 2 ms | 5 ms | 991 ms |
| k6, sends on a timer, timestamps in the payload | 2 ms | 877 ms | 987 ms |
| k6, sends after each reply | 2 ms | 4 ms | 1002 ms |
| Artillery | not reported | not reported | not reported |

A client that waits for each reply before sending the next one sends nothing
while the server is stalled, so only one request per connection records the
stall. The other requests that should have been sent in that second are never
measured, and p99 looks healthy. This is coordinated omission. k6 gets the
right answer when the script sends on a timer and carries a timestamp in
each message, as `k6-paced.js` does, but the common send-and-wait shape
(`k6-closed.js`) hides the stall. wsblast measures paced runs from each
request's scheduled time without any script, and reports round-trip time
next to it so you can see the difference. Artillery's WebSocket engine
counts messages but records no message latency.

k6's numbers have millisecond resolution, because its scripts time messages
with `Date.now()`.

## Reproducing

```bash
cargo install wsblast
# k6: https://grafana.com/docs/k6/latest/set-up/install-k6/
npm install -g artillery@2.0.34
scripts/compare/run.sh
```

Set `WSBLAST`, `K6` and `ARTILLERY` to use binaries that are not on `PATH`,
and `SERVER_CPU` and `TOOL_CPU` to choose the cores. The connection counts
and durations are variables at the top of the script. The script prints the
tables above as Markdown and keeps each tool's own report in
`scripts/compare/results/`.
