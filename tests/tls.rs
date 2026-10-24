//! `wss://` end to end: the compiled binary completes a rustls handshake
//! against an in-process TLS echo server whose self-signed certificate is
//! trusted through `SSL_CERT_FILE`, and rejects it when it is not trusted.

use futures_util::{SinkExt, StreamExt};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::process::Command;
use tokio_rustls::TlsAcceptor;
use tokio_rustls::rustls::ServerConfig;
use tokio_rustls::rustls::crypto::ring;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use tokio_tungstenite::tungstenite::protocol::Message;
use tokio_util::sync::CancellationToken;

struct TlsEchoServer {
    url: String,
    cert_pem: String,
    cancel: CancellationToken,
}

/// Spawns a `wss://localhost` echo server with a fresh self-signed certificate.
async fn spawn_tls_echo_server() -> TlsEchoServer {
    let certified = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
    let cert_pem = certified.cert.pem();
    let cert_der = CertificateDer::from(certified.cert.der().to_vec());
    let key_der = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(
        certified.signing_key.serialize_der(),
    ));

    let server_config = ServerConfig::builder_with_provider(Arc::new(ring::default_provider()))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![cert_der], key_der)
        .unwrap();
    let acceptor = TlsAcceptor::from(Arc::new(server_config));

    // Bind the address `localhost` resolves to first, which the client also
    // tries first. Windows runners resolve it to `::1` before `127.0.0.1`, and
    // Windows retries a refused loopback connection for about two seconds, so
    // a server on the other address would not be reached within the run.
    let local_addr = tokio::net::lookup_host("localhost:0")
        .await
        .unwrap()
        .next()
        .expect("localhost does not resolve");
    let listener = TcpListener::bind(local_addr).await.unwrap();
    let url = format!("wss://localhost:{}", listener.local_addr().unwrap().port());
    let cancel = CancellationToken::new();
    let cancel_server = cancel.clone();

    tokio::spawn(async move {
        loop {
            tokio::select! {
                Ok((stream, _)) = listener.accept() => {
                    let acceptor = acceptor.clone();
                    tokio::spawn(async move {
                        let Ok(tls_stream) = acceptor.accept(stream).await else {
                            return;
                        };
                        let Ok(ws_stream) = tokio_tungstenite::accept_async(tls_stream).await else {
                            return;
                        };
                        let (mut sink, mut reader) = ws_stream.split();
                        while let Some(Ok(msg)) = reader.next().await {
                            match msg {
                                Message::Text(_) | Message::Binary(_) => {
                                    let _ = sink.send(msg).await;
                                }
                                Message::Close(_) => {
                                    let _ = sink.send(Message::Close(None)).await;
                                    break;
                                }
                                _ => {}
                            }
                        }
                    });
                }
                _ = cancel_server.cancelled() => break,
            }
        }
    });

    TlsEchoServer {
        url,
        cert_pem,
        cancel,
    }
}

fn write_pem(name: &str, pem: &str) -> PathBuf {
    let path = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("wsblast-tls-{}-{name}.pem", std::process::id()));
    std::fs::write(&path, pem).unwrap();
    path
}

/// Runs a short echo test against `url`, trusting only the roots in `ca_file`.
async fn run_wsblast(url: &str, ca_file: &Path) -> (Option<i32>, serde_json::Value, String) {
    let child = Command::new(env!("CARGO_BIN_EXE_wsblast"))
        .args([
            url,
            "-c",
            "2",
            "-d",
            "1s",
            "--no-reconnect",
            "--no-progress",
            "--format",
            "json",
        ])
        .env("SSL_CERT_FILE", ca_file)
        .env_remove("SSL_CERT_DIR")
        .env("NO_COLOR", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();

    let output = tokio::time::timeout(Duration::from_secs(15), child.wait_with_output())
        .await
        .expect("wsblast did not exit within 15s")
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    let report = serde_json::from_str(&stdout)
        .unwrap_or_else(|err| panic!("stdout is not a JSON report ({err}):\n{stdout}\n{stderr}"));
    (output.status.code(), report, stderr)
}

fn metric(report: &serde_json::Value, pointer: &str) -> f64 {
    report
        .pointer(pointer)
        .and_then(serde_json::Value::as_f64)
        .unwrap_or_else(|| panic!("report has no numeric {pointer}:\n{report:#}"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wss_handshake_succeeds_with_a_trusted_certificate() {
    let server = spawn_tls_echo_server().await;
    let ca_file = write_pem("trusted", &server.cert_pem);

    let (code, report, stderr) = run_wsblast(&server.url, &ca_file).await;
    server.cancel.cancel();
    let _ = std::fs::remove_file(&ca_file);

    assert_eq!(code, Some(0), "report:\n{report:#}\nstderr:\n{stderr}");
    assert_eq!(
        metric(&report, "/metrics/total_connections_established"),
        2.0
    );
    assert!(metric(&report, "/metrics/total_messages_recv") > 0.0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wss_handshake_fails_when_the_certificate_is_not_trusted() {
    let server = spawn_tls_echo_server().await;
    let other_ca = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
    let ca_file = write_pem("untrusted", &other_ca.cert.pem());

    let (code, report, stderr) = run_wsblast(&server.url, &ca_file).await;
    server.cancel.cancel();
    let _ = std::fs::remove_file(&ca_file);

    assert_eq!(code, Some(1), "report:\n{report:#}\nstderr:\n{stderr}");
    assert_eq!(
        metric(&report, "/metrics/total_connections_established"),
        0.0
    );
    assert_eq!(
        metric(&report, "/metrics/error_breakdown/tls_handshake"),
        2.0
    );
}
