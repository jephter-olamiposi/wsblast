# `wsblast` WebSocket Benchmark Report

- **Targets:**
  - phase `warmup`: `ws://first.test/ws`
  - phase `spike`: `wss://second.test/ws?a=1&b=2`
- **Concurrency:** `50` workers
- **Connections:** `10` established / `0` failed / `0` reconnects
- **Duration:** `1.00s`
- **Throughput:** `100.00` msg/sec
- **Error Rate:** `0.00%`

### Latency Distribution

| Percentile | Handshake Latency | Message RTT Latency |
| :--- | :--- | :--- |
| **p50** | 500 µs | 250 µs |
| **p90** | 500 µs | 250 µs |
| **p95** | 500 µs | 250 µs |
| **p99** | 500 µs | 250 µs |
| **p99.9** | 500 µs | 250 µs |
| **Max** | 500 µs | 250 µs |

### SLO Gate Evaluation

| Metric | Target | Actual | Status |
| :--- | :--- | :--- | :--- |
| Latency p99 | `<= 1ms` | `250µs` | **✅ PASS** |
