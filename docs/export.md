# Live metrics export

wsblast can publish its live metrics while a run is in progress, so a long soak or a staged ramp can be watched on the same dashboards as the system under test. `--prometheus-listen ADDR` serves `GET /metrics` for Prometheus to scrape. It is part of the default build (`prometheus` feature).

[METHODOLOGY.md, section 6.4](../METHODOLOGY.md#64-live-export) defines each metric precisely. The report printed at the end of the run is still the record of the run: live values are for watching it happen.

Contents:

1. [Exported metrics](#1-exported-metrics)
2. [Prometheus](#2-prometheus)

## 1. Exported metrics

| Metric | Type |
| :--- | :--- |
| `wsblast_messages_sent_total` | counter |
| `wsblast_messages_received_total` | counter |
| `wsblast_bytes_sent_total` | counter |
| `wsblast_bytes_received_total` | counter |
| `wsblast_errors_total` | counter |
| `wsblast_connections_established_total` | counter |
| `wsblast_connections_failed_total` | counter |
| `wsblast_reconnects_total` | counter |
| `wsblast_active_connections` | gauge |
| `wsblast_run_elapsed_seconds` | gauge |
| `wsblast_latency_seconds{basis,quantile}` | gauge |
| `wsblast_latency_samples{basis}` | gauge |

The latency gauges hold p50, p90 and p99 (`quantile="0.5"`, `"0.9"`, `"0.99"`) of the most recently completed time-series interval, 1 second for runs of up to 10 minutes. `basis` is `response_time` for paced echo runs (`--rate`) and `rtt` otherwise. Until the first interval has ended, the latency families have no samples.

`wsblast_connections_failed_total` counts failed handshake attempts, including every failed reconnect attempt, so it keeps growing while a target is down. The report's `total_connections_failed` counts connection slots instead.

## 2. Prometheus

Start a run with an endpoint:

```bash
wsblast ws://127.0.0.1:9001/ws -c 200 -r 20 -d 10m --no-progress \
  --prometheus-listen 0.0.0.0:9464
```

wsblast binds the address before it opens any connection and prints it to stderr:

```text
Serving Prometheus metrics at http://0.0.0.0:9464/metrics
```

If the address cannot be bound, for example because the port is in use, wsblast exits with code 1 before the run starts. Use port 0 to let the system pick a free port; the line above then shows the port that was picked. The endpoint stops when the run ends, before the report is printed, so the last scrape may miss the final second of the run.

Check it by hand:

```bash
curl -s http://127.0.0.1:9464/metrics | grep -v '^#'
```

```text
wsblast_messages_sent_total 48213
wsblast_messages_received_total 48207
wsblast_bytes_sent_total 2652186
wsblast_bytes_received_total 2651856
wsblast_errors_total 0
wsblast_connections_established_total 200
wsblast_connections_failed_total 0
wsblast_reconnects_total 0
wsblast_active_connections 200
wsblast_run_elapsed_seconds 12.031
wsblast_latency_seconds{basis="response_time",quantile="0.5"} 0.000415
wsblast_latency_seconds{basis="response_time",quantile="0.9"} 0.000727
wsblast_latency_seconds{basis="response_time",quantile="0.99"} 0.00189
wsblast_latency_samples{basis="response_time"} 3998
```

### Scrape configuration

The latency gauges change once per interval, so a scrape interval of 1 to 5 seconds shows every change for runs of up to 10 minutes. A run is a short-lived process, so give the job a label you can filter on:

```yaml
# prometheus.yml
scrape_configs:
  - job_name: wsblast
    scrape_interval: 2s
    static_configs:
      - targets: ["loadgen-1:9464"]
        labels:
          test: checkout-soak
```

Useful queries:

```promql
# Messages per second, both directions
rate(wsblast_messages_sent_total[30s])
rate(wsblast_messages_received_total[30s])

# p99 latency of each completed interval
wsblast_latency_seconds{quantile="0.99"}

# Errors per second
rate(wsblast_errors_total[30s])
```

Percentiles from different intervals cannot be combined, so do not average or sum `wsblast_latency_seconds` over time to get a run-level percentile; the report has those.

### Running in a container

Publish the port and bind every interface inside the container:

```bash
docker run --rm -p 9464:9464 wsblast ws://target:9001/ws -d 5m --no-progress \
  --prometheus-listen 0.0.0.0:9464
```

### Scenarios

In a scenario (`--scenario`), the endpoint serves for the whole scenario. The counters keep counting across phases, and the latency gauges follow the phase in progress.
