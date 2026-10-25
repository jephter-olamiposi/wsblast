# `wsblast` Baseline Comparison Report

- **Threshold:** `±3.0%`

## ⚠ Regressions Detected

| Metric | Baseline | Current | Change |
| :--- | ---: | ---: | ---: |
| handshake_latency.p50_us | 1150.000 | 1200.000 | +4.35% |
| handshake_latency.p95_us | 4600.000 | 4800.000 | +4.35% |
| handshake_latency.p999_us | 18400.000 | 19200.000 | +4.35% |
| handshake_latency.p99_us | 10350.000 | 10800.000 | +4.35% |

## ✓ Improvements Detected

| Metric | Baseline | Current | Change |
| :--- | ---: | ---: | ---: |
| error_rate | 0.000 | 0.000 | -100.00% |
| errors.tcp_connect | 2.000 | 0.000 | -100.00% |
| errors.timeout | 30.000 | 0.000 | -100.00% |
| message_rtt.p50_us | 1100.000 | 800.000 | -27.27% |
| message_rtt.p95_us | 4400.000 | 3200.000 | -27.27% |
| message_rtt.p999_us | 17600.000 | 12800.000 | -27.27% |
| message_rtt.p99_us | 9900.000 | 7200.000 | -27.27% |
| throughput_bytes_per_sec | 514000.800 | 639872.017 | +24.49% |
| throughput_msg_per_sec | 4016.131 | 4999.333 | +24.48% |
| total_bytes_recv | 7710080.000 | 9598720.000 | +24.50% |
| total_bytes_sent | 7712000.000 | 9600000.000 | +24.48% |
| total_connections_established | 48.000 | 50.000 | +4.17% |
| total_connections_failed | 2.000 | 0.000 | -100.00% |
| total_messages_recv | 120470.000 | 149980.000 | +24.50% |
| total_messages_sent | 120500.000 | 150000.000 | +24.48% |


