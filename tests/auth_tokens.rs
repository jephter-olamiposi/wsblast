//! `--auth-type jwt` end to end: the compiled binary signs the tokens and an
//! in-process WebSocket server verifies each handshake's token before it
//! accepts the upgrade.
#![cfg(feature = "jwt")]

use futures_util::{SinkExt, StreamExt};
use std::path::PathBuf;
use std::process::{Output, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::net::TcpListener;
use tokio::process::Command;
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::http::StatusCode;
use tokio_tungstenite::tungstenite::protocol::Message;
use tokio_util::sync::CancellationToken;

type Verify = Arc<dyn Fn(&str) -> bool + Send + Sync>;

/// An echo server that checks `Authorization` on every handshake with
/// `verify`, rejects the upgrade with 401 when it fails, records every
/// token it saw and closes each session after `replies` echoes, so
/// workers reconnect.
struct VerifyingServer {
    url: String,
    seen: Arc<Mutex<Vec<(String, bool)>>>,
    cancel: CancellationToken,
}

impl VerifyingServer {
    async fn spawn(replies: usize, verify: Verify) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let cancel = CancellationToken::new();
        let (seen_server, cancel_server) = (Arc::clone(&seen), cancel.clone());

        tokio::spawn(async move {
            loop {
                tokio::select! {
                    Ok((stream, _)) = listener.accept() => {
                        let seen = Arc::clone(&seen_server);
                        let verify = Arc::clone(&verify);
                        tokio::spawn(async move {
                            // The callback's error type is fixed by tungstenite.
                            #[allow(clippy::result_large_err)]
                            let check = |req: &Request, resp: Response| -> Result<Response, ErrorResponse> {
                                let token = req
                                    .headers()
                                    .get("authorization")
                                    .and_then(|v| v.to_str().ok())
                                    .and_then(|v| v.strip_prefix("Bearer "))
                                    .unwrap_or_default()
                                    .to_string();
                                let valid = verify(&token);
                                seen.lock().unwrap().push((token, valid));
                                if valid {
                                    Ok(resp)
                                } else {
                                    let mut reject = ErrorResponse::new(None);
                                    *reject.status_mut() = StatusCode::UNAUTHORIZED;
                                    Err(reject)
                                }
                            };
                            let Ok(ws) = tokio_tungstenite::accept_hdr_async(stream, check).await
                            else {
                                return;
                            };
                            let (mut sink, mut reader) = ws.split();
                            let mut echoed = 0;
                            while let Some(Ok(msg)) = reader.next().await {
                                if let Message::Text(t) = msg {
                                    let _ = sink.send(Message::Text(t)).await;
                                    echoed += 1;
                                    if echoed == replies {
                                        let _ = sink.send(Message::Close(None)).await;
                                        break;
                                    }
                                }
                            }
                        });
                    }
                    _ = cancel_server.cancelled() => break,
                }
            }
        });

        Self { url, seen, cancel }
    }

    fn seen(&self) -> Vec<(String, bool)> {
        self.seen.lock().unwrap().clone()
    }
}

impl Drop for VerifyingServer {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

async fn wsblast(args: &[&str]) -> Output {
    let child = Command::new(env!("CARGO_BIN_EXE_wsblast"))
        .args(args)
        .env("NO_COLOR", "1")
        .env_remove("WSBLAST_JWT_KEY_FILE")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(30), child.wait_with_output())
        .await
        .expect("wsblast did not exit")
        .unwrap()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn write_file(name: &str, contents: &[u8]) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("auth-tokens");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    std::fs::write(&path, contents).unwrap();
    path
}

fn b64url_decode(input: &str) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let (mut acc, mut bits) = (0u32, 0);
    for c in input.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'-' => 62,
            b'_' => 63,
            _ => return None,
        };
        acc = (acc << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

/// Splits a compact JWS into its signing input, claims and signature.
fn split(token: &str) -> Option<(String, serde_json::Value, Vec<u8>)> {
    let (input, signature) = token.rsplit_once('.')?;
    let (_, payload) = input.split_once('.')?;
    let claims = serde_json::from_slice(&b64url_decode(payload)?).ok()?;
    Some((input.to_string(), claims, b64url_decode(signature)?))
}

/// Checks the time claims a server would check, with no leeway.
fn current(claims: &serde_json::Value) -> bool {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let claim = |name: &str| claims[name].as_u64();
    matches!(
        (claim("nbf"), claim("exp")),
        (Some(nbf), Some(exp)) if nbf <= now && now < exp
    )
}

#[tokio::test]
async fn hs256_tokens_pass_the_servers_signature_check() {
    let secret = b"integration-test-secret-0123456789abcdef";
    let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, secret);
    let verify: Verify = Arc::new(move |token| {
        split(token).is_some_and(|(input, claims, signature)| {
            ring::hmac::verify(&key, input.as_bytes(), &signature).is_ok()
                && current(&claims)
                && claims["sub"] == "load-test"
                && claims["aud"] == "ws"
        })
    });
    let server = VerifyingServer::spawn(20, verify).await;
    // `echo` style: the trailing newline is not part of the secret.
    let mut contents = secret.to_vec();
    contents.push(b'\n');
    let key_file = write_file("hs256.key", &contents);
    let output = wsblast(&[
        &server.url,
        "-c",
        "2",
        "-d",
        "2500ms",
        "-r",
        "100",
        "--no-progress",
        "--auth-type",
        "jwt",
        "--auth-jwt-key",
        key_file.to_str().unwrap(),
        "--auth-jwt-claims",
        r#"{"sub":"load-test","aud":"ws"}"#,
        "--auth-jwt-ttl",
        "2s",
    ])
    .await;

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let seen = server.seen();
    assert!(seen.len() >= 4, "too few handshakes: {seen:?}");
    assert!(seen.iter().all(|(_, valid)| *valid), "{seen:?}");
    // The run outlasts half the lifetime, so the token was re-signed
    // and reconnects picked the new one up.
    let distinct: std::collections::HashSet<_> = seen.iter().map(|(t, _)| t).collect();
    assert!(
        distinct.len() >= 2,
        "the token was never re-signed: {seen:?}"
    );
    assert!(!stderr(&output).contains(&seen[0].0));
}

#[tokio::test]
async fn rs256_per_connection_tokens_have_unique_jtis() {
    let pem = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/jwt/rs256-pkcs8.pem"
    ))
    .unwrap();
    let body: String = pem.lines().filter(|l| !l.starts_with("-----")).collect();
    let der = b64std_decode(&body);
    let pair = ring::signature::RsaKeyPair::from_pkcs8(&der).unwrap();
    let public = ring::signature::UnparsedPublicKey::new(
        &ring::signature::RSA_PKCS1_2048_8192_SHA256,
        ring::signature::KeyPair::public_key(&pair)
            .as_ref()
            .to_vec(),
    );
    let verify: Verify = Arc::new(move |token| {
        split(token).is_some_and(|(input, claims, signature)| {
            public.verify(input.as_bytes(), &signature).is_ok() && current(&claims)
        })
    });
    let server = VerifyingServer::spawn(10, verify).await;
    let output = wsblast(&[
        &server.url,
        "-c",
        "3",
        "-d",
        "800ms",
        "-r",
        "100",
        "--no-progress",
        "--auth-type",
        "jwt",
        "--auth-jwt-alg",
        "RS256",
        "--auth-jwt-key",
        concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/jwt/rs256-pkcs8.pem"
        ),
        "--auth-jwt-per-connection",
    ])
    .await;

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let seen = server.seen();
    assert!(seen.len() >= 6, "too few handshakes: {seen:?}");
    assert!(seen.iter().all(|(_, valid)| *valid), "{seen:?}");
    let jtis: std::collections::HashSet<String> = seen
        .iter()
        .map(|(token, _)| split(token).unwrap().1["jti"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(jtis.len(), seen.len(), "a jti was reused");
}

#[tokio::test]
async fn a_server_that_rejects_the_signature_fails_the_handshakes() {
    let verify: Verify = Arc::new(|_| false);
    let server = VerifyingServer::spawn(10, verify).await;
    let key_file = write_file("wrong.key", b"not-the-servers-secret");
    let output = wsblast(&[
        &server.url,
        "-d",
        "300ms",
        "--no-progress",
        "--auth-type",
        "jwt",
        "--auth-jwt-key",
        key_file.to_str().unwrap(),
    ])
    .await;
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(!server.seen().is_empty());
}

#[tokio::test]
async fn an_unusable_key_is_a_configuration_error_before_the_run() {
    let verify: Verify = Arc::new(|_| true);
    let server = VerifyingServer::spawn(10, verify).await;
    let key_file = write_file("not-pem.key", b"secret-key-material");
    let output = wsblast(&[
        &server.url,
        "-d",
        "300ms",
        "--no-progress",
        "--auth-type",
        "jwt",
        "--auth-jwt-alg",
        "ES256",
        "--auth-jwt-key",
        key_file.to_str().unwrap(),
    ])
    .await;
    assert_eq!(output.status.code(), Some(1));
    let stderr = stderr(&output);
    assert!(stderr.contains("not a PEM file"), "{stderr}");
    assert!(!stderr.contains("secret-key-material"), "{stderr}");
    assert!(server.seen().is_empty(), "the run started");
}

fn b64std_decode(input: &str) -> Vec<u8> {
    let url: String = input
        .trim_end_matches('=')
        .chars()
        .map(|c| match c {
            '+' => '-',
            '/' => '_',
            c => c,
        })
        .collect();
    b64url_decode(&url).unwrap()
}
