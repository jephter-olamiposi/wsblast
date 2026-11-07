//! `--compression` against `wsblast mock --compression`: what was agreed,
//! what went on the wire, and that every message arrives as it was sent.
#![cfg(all(feature = "mock", feature = "compression"))]

use std::time::Duration;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use wsblast::mock::{MockConfig, MockConfigBuilder, MockServer, MockStats};
use wsblast::{AggregatedMetrics, LoadTestConfig, LoadTestConfigBuilder, Runner, TestMode};

struct Mock {
    url: String,
    cancel: CancellationToken,
    task: JoinHandle<wsblast::Result<MockStats>>,
}

impl Mock {
    async fn start(builder: MockConfigBuilder) -> Self {
        let server = MockServer::bind(builder.listen("127.0.0.1:0").build().unwrap())
            .await
            .unwrap();
        let cancel = CancellationToken::new();
        Self {
            url: server.url(),
            task: tokio::spawn(server.run(cancel.clone())),
            cancel,
        }
    }

    async fn stop(self) -> MockStats {
        self.cancel.cancel();
        self.task.await.unwrap().unwrap()
    }
}

fn load(url: &str) -> LoadTestConfigBuilder {
    LoadTestConfig::builder(url)
        .connections(2)
        .duration(Duration::from_millis(800))
        .connect_timeout(Duration::from_secs(3))
        .message_timeout(Duration::from_secs(3))
}

async fn run(config: LoadTestConfigBuilder) -> AggregatedMetrics {
    Runner::new(config.build().unwrap())
        .with_signal_handling(false)
        .run()
        .await
        .unwrap()
}

/// A JSON message that compresses well, as chat and market data do.
fn repetitive_payload() -> String {
    let item = r#"{"symbol":"ABC","bid":101.25,"ask":101.27,"size":300}"#;
    format!("[{}]", vec![item; 40].join(","))
}

#[tokio::test]
async fn agreed_compression_shrinks_the_wire_and_keeps_every_message() {
    let mock = Mock::start(MockConfig::builder().compression(true)).await;
    let payload = repetitive_payload();
    let metrics = run(load(&mock.url)
        .compression(true)
        .payload_text(payload.clone())
        // Every reply must be the request, inflated back exactly.
        .expect(format!("contains:{payload}")))
    .await;
    let stats = mock.stop().await;

    assert_eq!(metrics.error_rate, 0.0, "{metrics:?}");
    assert!(metrics.total_messages_recv > 10, "{metrics:?}");
    let compression = metrics.compression.expect("a compression summary");
    assert_eq!(compression.connections_offered, 2);
    assert_eq!(compression.connections_negotiated, 2);
    assert_eq!(stats.connections_compressed, 2);
    // Byte totals are the messages' own sizes, compressed or not.
    assert_eq!(compression.bytes_recv, metrics.total_bytes_recv);
    assert!(compression.bytes_sent >= metrics.total_bytes_recv);
    // Context takeover makes repeated messages a few bytes each.
    assert!(
        compression.wire_bytes_sent * 10 < compression.bytes_sent,
        "{compression:?}"
    );
    assert!(
        compression.wire_bytes_recv * 10 < compression.bytes_recv,
        "{compression:?}"
    );
}

#[tokio::test]
async fn a_server_that_declines_is_served_uncompressed() {
    let mock = Mock::start(MockConfig::builder()).await;
    let metrics = run(load(&mock.url).compression(true)).await;
    let stats = mock.stop().await;

    assert_eq!(metrics.error_rate, 0.0, "{metrics:?}");
    assert!(metrics.total_messages_recv > 10, "{metrics:?}");
    let compression = metrics.compression.expect("a compression summary");
    assert_eq!(compression.connections_offered, 2);
    assert_eq!(compression.connections_negotiated, 0);
    assert_eq!(compression.wire_bytes_sent, 0);
    assert_eq!(stats.connections_compressed, 0);
}

#[tokio::test]
async fn a_client_that_does_not_offer_is_served_uncompressed() {
    let mock = Mock::start(MockConfig::builder().compression(true)).await;
    let metrics = run(load(&mock.url)).await;
    let stats = mock.stop().await;

    assert_eq!(metrics.error_rate, 0.0, "{metrics:?}");
    assert!(metrics.total_messages_recv > 10, "{metrics:?}");
    assert!(metrics.compression.is_none());
    assert_eq!(stats.connections_compressed, 0);
}

/// Messages far larger than a read buffer arrive in many reads, and
/// binary data that does not compress is still sent compressed.
#[tokio::test]
async fn large_and_incompressible_messages_round_trip() {
    let mock = Mock::start(MockConfig::builder().compression(true)).await;
    let mut state = 0x9e37_79b9_7f4a_7c15_u64;
    let noise: Vec<u8> = (0..200_000)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state as u8
        })
        .collect();
    let metrics = run(load(&mock.url)
        .compression(true)
        .payload_binary(noise.clone())
        // Unoptimized builds deflate slowly: the cap ends the run.
        .duration(Duration::from_secs(30))
        .max_requests(20))
    .await;
    mock.stop().await;

    assert_eq!(metrics.error_rate, 0.0, "{metrics:?}");
    // The cap ends the run as soon as the last message is sent, with each
    // connection's latest reply still on its way.
    assert!(metrics.total_messages_recv >= 18, "{metrics:?}");
    assert_eq!(
        metrics.total_bytes_recv,
        metrics.total_messages_recv * noise.len() as u64
    );
    let compression = metrics.compression.expect("a compression summary");
    assert_eq!(compression.connections_negotiated, 2);
    // Deflate adds a few bytes per block to data it cannot shrink.
    assert!(
        compression.wire_bytes_recv > compression.bytes_recv,
        "{compression:?}"
    );
    assert!(
        compression.wire_bytes_recv < compression.bytes_recv + compression.bytes_recv / 100,
        "{compression:?}"
    );
}

#[tokio::test]
async fn stream_mode_compresses_without_waiting_for_replies() {
    let mock = Mock::start(MockConfig::builder().compression(true)).await;
    let metrics = run(load(&mock.url)
        .compression(true)
        .mode(TestMode::Stream)
        .payload_text(repetitive_payload())
        .rate_per_connection(200))
    .await;
    mock.stop().await;

    assert_eq!(metrics.error_rate, 0.0, "{metrics:?}");
    assert!(metrics.total_messages_recv > 50, "{metrics:?}");
    let compression = metrics.compression.expect("a compression summary");
    assert_eq!(compression.connections_negotiated, 2);
    assert!(compression.wire_bytes_recv * 10 < compression.bytes_recv);
}

#[cfg(feature = "socketio")]
#[tokio::test]
async fn socketio_sessions_compress() {
    let mock = Mock::start(MockConfig::builder().socketio().compression(true)).await;
    let metrics = run(load(&mock.url)
        .compression(true)
        .mode(TestMode::SocketIo)
        .payload_text(repetitive_payload()))
    .await;
    mock.stop().await;

    assert_eq!(metrics.error_rate, 0.0, "{metrics:?}");
    assert!(metrics.message_rtt.count > 10, "{metrics:?}");
    let compression = metrics.compression.expect("a compression summary");
    assert_eq!(compression.connections_negotiated, 2);
    assert!(compression.wire_bytes_recv * 5 < compression.bytes_recv);
}
