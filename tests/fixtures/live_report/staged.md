# `wsblast` WebSocket Benchmark Report

- **Target:** `wss://staging.example.test/socket?room=a&b=1`
- **Concurrency:** `20` workers
- **Load Stages:** `5s:20, 15s:20, 5s:0`
- **Connections:** `18` established / `1` failed / `3` reconnects
- **Connections Interrupted:** `1` (still connecting at end of run)
- **Unsolicited Frames:** `5` skipped by `--match`
- **Duration:** `25.00s`
- **Throughput:** `399.84` msg/sec
- **Error Rate:** `0.49%`
- **Capture:** 20000 frames written, 7 dropped (`run.jsonl`)

### Latency Distribution

| Percentile | Handshake Latency | Message RTT Latency | Response Time (from schedule) |
| :--- | :--- | :--- | :--- |
| **p50** | 1.20 ms | 800 µs | 950 µs |
| **p90** | 3.60 ms | 2.40 ms | 2.85 ms |
| **p95** | 4.80 ms | 3.20 ms | 3.80 ms |
| **p99** | 9.60 ms | 6.40 ms | 7.60 ms |
| **p99.9** | 18.00 ms | 12.00 ms | 14.25 ms |
| **Max** | 24.00 ms | 16.00 ms | 19.00 ms |

### Over Time

| Time Window | Throughput (msg/s) | Errors | Worst Response Time p99 |
| :--- | :--- | :--- | :--- |
| 0s to 3s | 400.0 | 0 | 2.40 ms |
| 3s to 6s | 400.0 | 0 | 2.40 ms |
| 6s to 9s | 400.0 | 0 | 2.40 ms |
| 9s to 12s | 400.0 | 0 | 2.40 ms |
| 12s to 15s | 400.0 | 2 | 310.00 ms |
| 15s to 18s | 400.0 | 0 | 2.40 ms |
| 18s to 21s | 400.0 | 0 | 2.40 ms |
| 21s to 24s | 400.0 | 0 | 2.40 ms |
| 24s to 25s | 400.0 | 0 | 2.40 ms |

