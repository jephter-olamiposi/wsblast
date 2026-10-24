//! wsblast - High-performance WebSocket load testing CLI with percentile latency & CI gating.

#[tokio::main]
async fn main() {
    wsblast::__private::run_cli().await;
}
