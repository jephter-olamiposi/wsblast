# Live metrics export

wsblast can publish its live metrics while a run is in progress, so a long soak or a staged ramp can be watched on the same dashboards as the system under test:

- `--prometheus-listen ADDR` serves `GET /metrics` for Prometheus to scrape. It is part of the default build (`prometheus` feature).
- `--otel-endpoint URL` pushes the same metrics to an OpenTelemetry collector over OTLP/HTTP with protobuf bodies. It needs the opt-in `otel` feature.

Both export the same set of metrics. [METHODOLOGY.md, section 6.4](../METHODOLOGY.md#64-live-export) defines each one precisely. The report printed at the end of the run is still the record of the run: live values are for watching it happen.

Contents:

1. [Exported metrics](#1-exported-metrics)
2. [Prometheus](#2-prometheus)
3. [OpenTelemetry](#3-opentelemetry)
4. [Both at once](#4-both-at-once)

## 1. Exported metrics

| Prometheus | OpenTelemetry | Type |
| :--- | :--- | :--- |
| `wsblast_messages_sent_total` | `wsblast.messages.sent` | counter |
| `wsblast_messages_received_total` | `wsblast.messages.received` | counter |
| `wsblast_bytes_sent_total` | `wsblast.bytes.sent` | counter |
| `wsblast_bytes_received_total` | `wsblast.bytes.received` | counter |
| `wsblast_errors_total` | `wsblast.errors` | counter |
| `wsblast_connections_established_total` | `wsblast.connections.established` | counter |
| `wsblast_connections_failed_total` | `wsblast.connections.failed` | counter |
| `wsblast_reconnects_total` | `wsblast.reconnects` | counter |
| `wsblast_active_connections` | `wsblast.connections.active` | gauge |
| `wsblast_run_elapsed_seconds` | `wsblast.run.elapsed` | gauge |
| `wsblast_latency_seconds{basis,quantile}` | `wsblast.latency` | gauge |
| `wsblast_latency_samples{basis}` | `wsblast.latency.samples` | gauge |

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

## 3. OpenTelemetry

The `otel` feature is not in the default build because it adds the OpenTelemetry SDK and an HTTP client. Build with it:

```bash
cargo install wsblast --locked --features otel
```

Then point wsblast at a collector's OTLP/HTTP receiver, usually on port 4318:

```bash
wsblast ws://127.0.0.1:9001/ws -c 200 -r 20 -d 10m --no-progress \
  --otel-endpoint http://localhost:4318 \
  --otel-interval 5s \
  --otel-service-name checkout-soak
```

| Flag | Default | Meaning |
| :--- | :--- | :--- |
| `--otel-endpoint URL` | none (export off) | OTLP/HTTP base URL. wsblast posts to `URL/v1/metrics`; a URL that already ends in `/v1/metrics` is used as given. `http://` and `https://` are accepted; HTTPS uses the platform's root certificates, or `SSL_CERT_FILE` / `SSL_CERT_DIR`. |
| `--otel-interval DURATION` | `10s` | Time between exports. One more export is sent when the run ends, so a run shorter than the interval still exports its final values. |
| `--otel-service-name NAME` | `$OTEL_SERVICE_NAME`, else `wsblast` | The `service.name` resource attribute. |

Every data point carries `server.address` (the target URL's host) and `wsblast.mode` (`echo`, `stream`, `listen` or `graphql-ws`); the latency points also carry `basis` and `quantile`. Counters are cumulative. `OTEL_EXPORTER_OTLP_HEADERS` adds request headers, for example an API key for a hosted backend, and `OTEL_RESOURCE_ATTRIBUTES` adds resource attributes:

```bash
OTEL_EXPORTER_OTLP_HEADERS="x-api-key=${OTLP_KEY}" \
OTEL_RESOURCE_ATTRIBUTES="deployment.environment=staging" \
  wsblast wss://api.example.com/ws -d 5m --otel-endpoint https://otlp.example.com
```

`OTEL_EXPORTER_OTLP_ENDPOINT` is not read: export is on only when `--otel-endpoint` is given.

An export that fails (collector down, wrong URL, rejected request) prints one warning and the run carries on; its report and exit code are unaffected. Each export is given at most 5 seconds (or the interval, if shorter) and is not retried, because the next export carries the cumulative values again.

### Collector configuration

A collector that receives OTLP/HTTP and exposes the metrics to Prometheus:

```yaml
# otel-collector.yaml
receivers:
  otlp:
    protocols:
      http:
        endpoint: 0.0.0.0:4318

processors:
  batch: {}

exporters:
  prometheus:
    endpoint: 0.0.0.0:8889
  debug:
    verbosity: basic

service:
  pipelines:
    metrics:
      receivers: [otlp]
      processors: [batch]
      exporters: [prometheus, debug]
```

```bash
docker run --rm -p 4318:4318 -p 8889:8889 \
  -v "$PWD/otel-collector.yaml:/etc/otelcol-contrib/config.yaml" \
  otel/opentelemetry-collector-contrib:latest
```

The collector's Prometheus exporter turns the dotted names into Prometheus names and adds unit and `_total` suffixes, so `wsblast.messages.sent` appears as `wsblast_messages_sent_total` and `wsblast.bytes.sent` as `wsblast_bytes_sent_bytes_total`. Names from the collector can therefore differ slightly from those of `--prometheus-listen`.

## 4. Both at once

The two exporters are independent and can run together; both read the same values:

```bash
wsblast ws://127.0.0.1:9001/ws -d 30m --no-progress \
  --prometheus-listen 127.0.0.1:9464 \
  --otel-endpoint http://localhost:4318
```

In a scenario (`--scenario`), both run for the whole scenario. The counters keep counting across phases, and the latency gauges follow the phase in progress.
