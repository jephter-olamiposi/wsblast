# `wsblast` Baseline Comparison Report

- **Threshold:** `±3.0%`

## ⚠ Regressions Detected

| Metric | Baseline | Current | Change |
| :--- | ---: | ---: | ---: |
| error_rate | 0.000 | 0.000 | +100.00% |
| errors.tcp_connect | 0.000 | 2.000 | +100.00% |
| errors.timeout | 0.000 | 30.000 | +100.00% |
| message_rtt.p50_us | 800.000 | 1100.000 | +37.50% |
| message_rtt.p95_us | 3200.000 | 4400.000 | +37.50% |
| message_rtt.p999_us | 12800.000 | 17600.000 | +37.50% |
| message_rtt.p99_us | 7200.000 | 9900.000 | +37.50% |
| throughput_bytes_per_sec | 639872.017 | 514000.800 | -19.67% |
| throughput_msg_per_sec | 4999.333 | 4016.131 | -19.67% |
| total_bytes_recv | 9598720.000 | 7710080.000 | -19.68% |
| total_bytes_sent | 9600000.000 | 7712000.000 | -19.67% |
| total_connections_established | 50.000 | 48.000 | -4.00% |
| total_connections_failed | 0.000 | 2.000 | +100.00% |
| total_messages_recv | 149980.000 | 120470.000 | -19.68% |
| total_messages_sent | 150000.000 | 120500.000 | -19.67% |

## ✓ Improvements Detected

| Metric | Baseline | Current | Change |
| :--- | ---: | ---: | ---: |
| handshake_latency.p50_us | 1200.000 | 1150.000 | -4.17% |
| handshake_latency.p95_us | 4800.000 | 4600.000 | -4.17% |
| handshake_latency.p999_us | 19200.000 | 18400.000 | -4.17% |
| handshake_latency.p99_us | 10800.000 | 10350.000 | -4.17% |


