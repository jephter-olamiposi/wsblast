//! `--auth-type jwt` and `--auth-type oauth2` end to end: the compiled
//! binary signs or fetches the tokens, an in-process token endpoint issues
//! OAuth2 tokens, and an in-process WebSocket server checks each
//! handshake's token before it accepts the upgrade.
#![cfg(any(feature = "jwt", feature = "oauth2"))]

use futures_util::{SinkExt, StreamExt};
use std::path::PathBuf;
use std::process::{Output, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;
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

#[cfg(feature = "jwt")]
async fn wsblast(args: &[&str]) -> Output {
    wsblast_with_env(args, &[]).await
}

async fn wsblast_with_env(args: &[&str], env: &[(&str, &str)]) -> Output {
    let child = Command::new(env!("CARGO_BIN_EXE_wsblast"))
        .args(args)
        .env("NO_COLOR", "1")
        .env_remove("WSBLAST_JWT_KEY_FILE")
        .env_remove("WSBLAST_OAUTH2_CLIENT_SECRET")
        .envs(env.iter().copied())
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

#[cfg(feature = "jwt")]
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

#[cfg(feature = "jwt")]
/// Splits a compact JWS into its signing input, claims and signature.
fn split(token: &str) -> Option<(String, serde_json::Value, Vec<u8>)> {
    let (input, signature) = token.rsplit_once('.')?;
    let (_, payload) = input.split_once('.')?;
    let claims = serde_json::from_slice(&b64url_decode(payload)?).ok()?;
    Some((input.to_string(), claims, b64url_decode(signature)?))
}

#[cfg(feature = "jwt")]
/// Checks the time claims a server would check, with no leeway.
fn current(claims: &serde_json::Value) -> bool {
    use std::time::{SystemTime, UNIX_EPOCH};
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

#[cfg(feature = "jwt")]
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

#[cfg(feature = "jwt")]
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

#[cfg(feature = "jwt")]
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

#[cfg(feature = "jwt")]
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

#[cfg(feature = "jwt")]
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

#[cfg(feature = "oauth2")]
mod oauth2 {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// One request the token endpoint received.
    #[derive(Debug, Clone)]
    struct TokenRequest {
        request_line: String,
        headers: Vec<(String, String)>,
        body: String,
    }

    impl TokenRequest {
        fn header(&self, name: &str) -> Option<&str> {
            self.headers
                .iter()
                .find(|(n, _)| n.eq_ignore_ascii_case(name))
                .map(|(_, v)| v.as_str())
        }
    }

    type Respond = Arc<dyn Fn(usize) -> (&'static str, String) + Send + Sync>;

    /// A minimal HTTP/1.1 token endpoint: one request per connection,
    /// answered by `respond` with the request's number (from 1).
    struct TokenServer {
        url: String,
        requests: Arc<Mutex<Vec<TokenRequest>>>,
        cancel: CancellationToken,
    }

    impl TokenServer {
        async fn spawn(respond: Respond) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!(
                "http://127.0.0.1:{}/oauth/token",
                listener.local_addr().unwrap().port()
            );
            let requests = Arc::new(Mutex::new(Vec::new()));
            let cancel = CancellationToken::new();
            let (requests_server, cancel_server) = (Arc::clone(&requests), cancel.clone());
            tokio::spawn(async move {
                loop {
                    tokio::select! {
                        Ok((mut stream, _)) = listener.accept() => {
                            let requests = Arc::clone(&requests_server);
                            let respond = Arc::clone(&respond);
                            tokio::spawn(async move {
                                let Some(request) = read_request(&mut stream).await else {
                                    return;
                                };
                                let number = {
                                    let mut requests = requests.lock().unwrap();
                                    requests.push(request);
                                    requests.len()
                                };
                                let (status, body) = respond(number);
                                let response = format!(
                                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\n\
                                     Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                                    body.len()
                                );
                                let _ = stream.write_all(response.as_bytes()).await;
                                let _ = stream.shutdown().await;
                            });
                        }
                        _ = cancel_server.cancelled() => break,
                    }
                }
            });
            Self {
                url,
                requests,
                cancel,
            }
        }

        fn requests(&self) -> Vec<TokenRequest> {
            self.requests.lock().unwrap().clone()
        }
    }

    impl Drop for TokenServer {
        fn drop(&mut self) {
            self.cancel.cancel();
        }
    }

    async fn read_request(stream: &mut tokio::net::TcpStream) -> Option<TokenRequest> {
        let mut buf = Vec::new();
        let head_end = loop {
            let mut chunk = [0u8; 1024];
            let n = stream.read(&mut chunk).await.ok()?;
            if n == 0 {
                return None;
            }
            buf.extend_from_slice(&chunk[..n]);
            if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                break i;
            }
        };
        let head = String::from_utf8(buf[..head_end].to_vec()).ok()?;
        let mut lines = head.split("\r\n");
        let request_line = lines.next()?.to_string();
        let headers: Vec<(String, String)> = lines
            .filter_map(|line| {
                let (name, value) = line.split_once(':')?;
                Some((name.trim().to_string(), value.trim().to_string()))
            })
            .collect();
        let length: usize = headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case("content-length"))
            .and_then(|(_, v)| v.parse().ok())
            .unwrap_or(0);
        let mut body = buf[head_end + 4..].to_vec();
        while body.len() < length {
            let mut chunk = [0u8; 1024];
            let n = stream.read(&mut chunk).await.ok()?;
            if n == 0 {
                return None;
            }
            body.extend_from_slice(&chunk[..n]);
        }
        Some(TokenRequest {
            request_line,
            headers,
            body: String::from_utf8(body).ok()?,
        })
    }

    fn base_args<'a>(ws: &'a str, tokens: &'a str) -> Vec<&'a str> {
        vec![
            ws,
            "--no-progress",
            "--auth-type",
            "oauth2",
            "--auth-oauth2-token-url",
            tokens,
            "--auth-oauth2-client-id",
            "load test",
        ]
    }

    #[tokio::test]
    async fn the_grant_request_is_well_formed_and_the_token_reaches_the_server() {
        let tokens = TokenServer::spawn(Arc::new(|_| {
            (
                "200 OK",
                r#"{"access_token":"at-1","token_type":"Bearer","expires_in":3600}"#.into(),
            )
        }))
        .await;
        let ws = VerifyingServer::spawn(1000, Arc::new(|token| token == "at-1")).await;
        let secret = write_file("oauth2-secret", b"s3cr+t:x\n");
        let mut args = base_args(&ws.url, &tokens.url);
        args.extend([
            "-c",
            "2",
            "-d",
            "500ms",
            "--auth-oauth2-client-secret-file",
            secret.to_str().unwrap(),
            "--auth-oauth2-scope",
            "ws.read ws.write",
            "--auth-oauth2-audience",
            "https://api.example",
        ]);
        let output = wsblast_with_env(&args, &[]).await;

        assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
        let seen = ws.seen();
        assert!(seen.len() >= 2, "{seen:?}");
        assert!(seen.iter().all(|(_, valid)| *valid), "{seen:?}");

        let requests = tokens.requests();
        assert_eq!(requests.len(), 1, "{requests:?}");
        let request = &requests[0];
        assert_eq!(request.request_line, "POST /oauth/token HTTP/1.1");
        // RFC 6749 section 2.3.1: the id and secret are form-encoded, then
        // sent as Basic credentials: base64("load+test:s3cr%2Bt%3Ax").
        assert_eq!(
            request.header("authorization"),
            Some("Basic bG9hZCt0ZXN0OnMzY3IlMkJ0JTNBeA==")
        );
        assert_eq!(
            request.header("content-type"),
            Some("application/x-www-form-urlencoded")
        );
        assert_eq!(request.header("accept"), Some("application/json"));
        assert_eq!(
            request.body,
            "grant_type=client_credentials&scope=ws.read+ws.write&audience=https%3A%2F%2Fapi.example"
        );
        assert!(!stderr(&output).contains("at-1"));
    }

    #[tokio::test]
    async fn a_rejected_grant_fails_before_the_run_with_the_status_and_error() {
        let tokens = TokenServer::spawn(Arc::new(|_| {
            (
                "401 Unauthorized",
                r#"{"error":"invalid_client","error_description":"Client authentication failed"}"#
                    .into(),
            )
        }))
        .await;
        let ws = VerifyingServer::spawn(1000, Arc::new(|_| true)).await;
        let mut args = base_args(&ws.url, &tokens.url);
        args.extend(["-d", "300ms"]);
        let output = wsblast_with_env(
            &args,
            &[("WSBLAST_OAUTH2_CLIENT_SECRET", "super-secret-value")],
        )
        .await;

        assert_eq!(output.status.code(), Some(1));
        let stderr = stderr(&output);
        assert!(
            stderr.contains("Authentication token request failed"),
            "{stderr}"
        );
        assert!(stderr.contains("401 Unauthorized"), "{stderr}");
        assert!(stderr.contains("invalid_client"), "{stderr}");
        assert!(!stderr.contains("super-secret-value"), "{stderr}");
        assert_eq!(tokens.requests().len(), 1);
        assert!(ws.seen().is_empty(), "the run started");
    }

    #[tokio::test]
    async fn tokens_are_refreshed_before_they_expire_and_reconnects_use_them() {
        let tokens = TokenServer::spawn(Arc::new(|n| {
            (
                "200 OK",
                format!(r#"{{"access_token":"at-{n}","token_type":"bearer","expires_in":2}}"#),
            )
        }))
        .await;
        // Sessions end after 20 replies, so the worker reconnects often.
        let ws = VerifyingServer::spawn(20, Arc::new(|token| token.starts_with("at-"))).await;
        let mut args = base_args(&ws.url, &tokens.url);
        args.extend(["-c", "1", "-d", "2500ms", "-r", "100"]);
        let output = wsblast_with_env(&args, &[("WSBLAST_OAUTH2_CLIENT_SECRET", "s3cret")]).await;

        assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
        let requests = tokens.requests();
        assert!(
            requests.len() >= 2,
            "the token was not refreshed: {requests:?}"
        );
        let numbers: Vec<usize> = ws
            .seen()
            .iter()
            .map(|(token, valid)| {
                assert!(valid, "{token}");
                token.strip_prefix("at-").unwrap().parse().unwrap()
            })
            .collect();
        assert!(numbers.len() >= 3, "too few handshakes: {numbers:?}");
        assert_eq!(numbers[0], 1, "{numbers:?}");
        assert!(numbers.windows(2).all(|w| w[0] <= w[1]), "{numbers:?}");
        assert!(
            *numbers.last().unwrap() >= 2,
            "reconnects did not pick up the refreshed token: {numbers:?}"
        );
    }

    #[tokio::test]
    async fn a_failed_refresh_keeps_the_previous_token_and_warns() {
        let tokens = TokenServer::spawn(Arc::new(|n| match n {
            1 => (
                "200 OK",
                r#"{"access_token":"at-1","expires_in":2}"#.to_string(),
            ),
            _ => (
                "503 Service Unavailable",
                r#"{"error":"temporarily_unavailable"}"#.to_string(),
            ),
        }))
        .await;
        let ws = VerifyingServer::spawn(20, Arc::new(|token| token == "at-1")).await;
        let mut args = base_args(&ws.url, &tokens.url);
        args.extend(["-c", "1", "-d", "2500ms", "-r", "100"]);
        let output = wsblast_with_env(&args, &[("WSBLAST_OAUTH2_CLIENT_SECRET", "s3cret")]).await;

        assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
        assert!(tokens.requests().len() >= 2);
        let seen = ws.seen();
        assert!(seen.len() >= 3, "{seen:?}");
        assert!(seen.iter().all(|(_, valid)| *valid), "{seen:?}");
        let stderr = stderr(&output);
        assert_eq!(
            stderr.matches("OAuth2 token refresh failed").count(),
            1,
            "{stderr}"
        );
        assert!(
            stderr.contains("503 Service Unavailable: temporarily_unavailable"),
            "{stderr}"
        );
        assert!(stderr.contains("keep the previous token"), "{stderr}");
    }
}
