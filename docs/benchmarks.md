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

- wsblast 0.2.4, k6 v1.8.2, Artillery 2.0.34
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
| wsblast | 157,219 | 29.2 s | 161,471 |
| k6 | 29,314 | 29.7 s | 29,630 |
| Artillery | 17,563 | 33.8 s | 15,612 |

Each tool uses its whole core, so the first column is what one core of each
can drive against a server that keeps up. The last column is the comparison
between the tools: replies divided by the CPU time the tool used. It
includes startup, which costs Artillery about 2.8 seconds of CPU and the
others under 0.2 seconds. Without it, Artillery's figure would be about
17,000. wsblast 0.2.2 reached 71,496 replies per CPU second on this test; 0.2.3
reads with smaller buffers and parses payload templates once
([CHANGELOG.md](../CHANGELOG.md)). 0.2.4 opens TLS itself and can add a
compression layer under the WebSocket library; run back to back with 0.2.3
on this machine, it was within run-to-run noise of it (about 163,000 replies
per CPU second against 158,000, three runs each).

## 2. Memory per connection

Peak resident memory of the tool's processes with 500 and with 2,000
connections, each sending one message a second for 20 seconds. The last
column is the slope between the two runs, so fixed costs cancel out.

| Tool | Peak at 500 | Peak at 2,000 | Per additional connection |
| :--- | ---: | ---: | ---: |
| wsblast | 20 MB | 38 MB | 12 KB |
| k6 | 162 MB | 535 MB | 255 KB |
| Artillery | 323 MB | 342 MB | 12 KB |

wsblast's read buffers share a budget of 16 MiB that is not all used at
these counts (see [METHODOLOGY.md](../METHODOLOGY.md), section 10); 0.2.2
needed 102 MB for 2,000 connections. These connections are uncompressed;
with `--compression` each one also holds a deflater and an inflater, about
150 KB (METHODOLOGY.md, section 7.3). k6 runs a JavaScript runtime for each
virtual user, which is most of its cost per connection. Artillery's Node.js
process starts large and then grows slowly.

## 3. A server that stalls

The server stops replying for 1 second every 10 seconds. Ten connections send
20 messages a second each, for 30 seconds. About 10% of the requests are due
while the server is stalled, so a correct p99 is close to a second.

| Tool and metric | p50 | p99 | max |
| :--- | ---: | ---: | ---: |
| wsblast, response time (from schedule) | 0 ms | 892 ms | 993 ms |
| wsblast, round-trip time | 0 ms | 1 ms | 993 ms |
| k6, sends on a timer, timestamps in the payload | 0 ms | 869 ms | 996 ms |
| k6, sends after each reply | 0 ms | 3 ms | 995 ms |
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
with `Date.now()`, so the table rounds every value to milliseconds. The mock
answers in well under a millisecond outside its stalls, which puts every
median at 0 ms.

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
