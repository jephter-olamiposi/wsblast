//! Test servers and token helpers shared by integration tests: a WebSocket
//! echo server that checks each handshake's token, a minimal OAuth2 token
//! endpoint, and a JWT parser.
//!
//! Each integration test file is its own crate and uses only part of this
//! module, so items unused by one of them are expected.
#![allow(dead_code)]

use futures_util::{SinkExt, StreamExt};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::http::StatusCode;
use tokio_tungstenite::tungstenite::protocol::Message;
use tokio_util::sync::CancellationToken;

/// Decides whether the bearer token of a handshake is valid.
pub type Verify = Arc<dyn Fn(&str) -> bool + Send + Sync>;

/// An echo server that checks `Authorization` on every handshake with
/// `verify`, rejects the upgrade with 401 when it fails, records every
/// token it saw and closes each session after `replies` echoes, so
/// workers reconnect.
pub struct VerifyingServer {
    pub url: String,
    seen: Arc<Mutex<Vec<(String, bool)>>>,
    cancel: CancellationToken,
}

impl VerifyingServer {
    pub async fn spawn(replies: usize, verify: Verify) -> Self {
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

    /// Every token a handshake presented, in arrival order, and whether it
    /// was valid.
    pub fn seen(&self) -> Vec<(String, bool)> {
        self.seen.lock().unwrap().clone()
    }
}

impl Drop for VerifyingServer {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

/// Writes `contents` to `name` in the tests' scratch directory.
pub fn write_file(name: &str, contents: &[u8]) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("test-files");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    std::fs::write(&path, contents).unwrap();
    path
}

/// Decodes unpadded base64url.
pub fn b64url_decode(input: &str) -> Option<Vec<u8>> {
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
pub fn split_jwt(token: &str) -> Option<(String, serde_json::Value, Vec<u8>)> {
    let (input, signature) = token.rsplit_once('.')?;
    let (_, payload) = input.split_once('.')?;
    let claims = serde_json::from_slice(&b64url_decode(payload)?).ok()?;
    Some((input.to_string(), claims, b64url_decode(signature)?))
}

/// Checks the time claims a server would check, with no leeway.
pub fn jwt_is_current(claims: &serde_json::Value) -> bool {
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

/// One request the token endpoint received.
#[derive(Debug, Clone)]
pub struct TokenRequest {
    pub request_line: String,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

/// Answers the token request with the given number (from 1): an HTTP status
/// line and a JSON body.
pub type Respond = Arc<dyn Fn(usize) -> (&'static str, String) + Send + Sync>;

/// A minimal HTTP/1.1 OAuth2 token endpoint: one request per connection,
/// answered by `respond` with the request's number.
pub struct TokenServer {
    pub url: String,
    requests: Arc<Mutex<Vec<TokenRequest>>>,
    cancel: CancellationToken,
}

impl TokenServer {
    pub async fn spawn(respond: Respond) -> Self {
        use tokio::io::AsyncWriteExt;
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

    /// Every request received so far, in arrival order.
    pub fn requests(&self) -> Vec<TokenRequest> {
        self.requests.lock().unwrap().clone()
    }
}

impl Drop for TokenServer {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

async fn read_request(stream: &mut tokio::net::TcpStream) -> Option<TokenRequest> {
    use tokio::io::AsyncReadExt;
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
