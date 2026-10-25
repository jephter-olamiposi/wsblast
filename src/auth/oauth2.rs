//! `--auth-type oauth2`: an access token from the OAuth 2.0
//! client-credentials grant (RFC 6749 section 4.4), fetched before the run
//! and again before it expires.
//!
//! The client authenticates with HTTP Basic (RFC 6749 section 2.3.1), which
//! every authorization server must support; the client id and secret are
//! form-urlencoded first, as that section requires. The request runs on a
//! plain thread that owns the blocking HTTP client, because the client
//! must not be created, used or dropped on a Tokio worker thread.

use super::{LiveAuthHeader, RefreshGuard};
use crate::cli::Cli;
use crate::config::{AuthScheme, Secret};
use crate::error::{ConfigError, Result, WsBlastError};
use http::StatusCode;
use http::header::{ACCEPT, CONTENT_TYPE, HeaderName, HeaderValue};
use serde_json::{Map, Value};
use std::io::Read;
use std::sync::mpsc;
use std::time::Duration;
use url::{Host, Url, form_urlencoded};

/// Environment variable read for the client secret when
/// `--auth-oauth2-client-secret-file` is not given.
pub(crate) const CLIENT_SECRET_ENV: &str = "WSBLAST_OAUTH2_CLIENT_SECRET";

/// Time allowed for each token request.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Shortest wait between two token requests.
const MIN_REFRESH_DELAY: Duration = Duration::from_secs(1);

/// Longest wait before retrying a failed refresh.
const RETRY_DELAY: Duration = Duration::from_secs(5);

/// Largest token response read; a longer one is rejected.
const MAX_RESPONSE: u64 = 64 * 1024;

/// The validated `--auth-oauth2-*` options.
#[derive(Debug, Clone)]
pub(crate) struct OAuth2Config {
    pub(crate) token_url: Url,
    pub(crate) client_id: String,
    pub(crate) client_secret: Secret,
    pub(crate) scope: Option<String>,
    pub(crate) audience: Option<String>,
}

/// Validates the `--auth-oauth2-*` options of `--auth-type oauth2` and reads
/// the client secret.
pub(crate) fn config_from_cli(cli: &Cli) -> Result<OAuth2Config> {
    let invalid = |message: String| WsBlastError::from(ConfigError::InvalidAuth(message));
    let token_url = cli
        .auth_oauth2_token_url
        .as_deref()
        .ok_or_else(|| invalid("--auth-type oauth2 requires --auth-oauth2-token-url".into()))?;
    let token_url = Url::parse(token_url)
        .map_err(|e| invalid(format!("--auth-oauth2-token-url is not a valid URL: {e}")))?;
    match token_url.scheme() {
        "https" => {}
        "http" if is_loopback(&token_url) => {}
        _ => {
            return Err(invalid(
                "--auth-oauth2-token-url must be an https:// URL; plain http:// is accepted only \
                 for loopback addresses"
                    .into(),
            ));
        }
    }

    let client_id = cli
        .auth_oauth2_client_id
        .clone()
        .filter(|id| !id.is_empty())
        .ok_or_else(|| invalid("--auth-type oauth2 requires --auth-oauth2-client-id".into()))?;

    let client_secret = match &cli.auth_oauth2_client_secret_file {
        Some(path) => std::fs::read_to_string(path)
            .map_err(|e| {
                invalid(format!(
                    "could not read --auth-oauth2-client-secret-file {}: {e}",
                    path.display()
                ))
            })?
            .trim_end_matches(['\r', '\n'])
            .to_string(),
        None => std::env::var(CLIENT_SECRET_ENV).unwrap_or_default(),
    };
    if client_secret.is_empty() {
        return Err(invalid(format!(
            "--auth-type oauth2 requires a client secret in --auth-oauth2-client-secret-file FILE \
             or {CLIENT_SECRET_ENV}"
        )));
    }

    let non_empty = |value: &Option<String>| value.clone().filter(|v| !v.is_empty());
    Ok(OAuth2Config {
        token_url,
        client_id,
        client_secret: Secret::new(client_secret),
        scope: non_empty(&cli.auth_oauth2_scope),
        audience: non_empty(&cli.auth_oauth2_audience),
    })
}

fn is_loopback(url: &Url) -> bool {
    match url.host() {
        Some(Host::Ipv4(ip)) => ip.is_loopback(),
        Some(Host::Ipv6(ip)) => ip.is_loopback(),
        Some(Host::Domain(domain)) => domain.eq_ignore_ascii_case("localhost"),
        None => false,
    }
}

/// Fetches the first token before the run and starts the thread that
/// fetches a new one before each token expires.
pub(super) fn prepare(
    config: &OAuth2Config,
    name: HeaderName,
) -> std::result::Result<(LiveAuthHeader, RefreshGuard), ConfigError> {
    let (stop, stopped) = mpsc::channel::<()>();
    let (first_tx, first_rx) = mpsc::channel();
    let config = config.clone();
    std::thread::spawn(move || {
        let client = match client() {
            Ok(client) => client,
            Err(err) => {
                let _ = first_tx.send(Err(err));
                return;
            }
        };
        let (value, expires_in) = match fetch_header(&client, &config, &name) {
            Ok(first) => first,
            Err(err) => {
                let _ = first_tx.send(Err(err));
                return;
            }
        };
        let live = LiveAuthHeader::new(name.clone(), value);
        let _ = first_tx.send(Ok(live.clone()));
        // A token without expires_in is used for the whole run.
        let Some(expires_in) = expires_in else {
            return;
        };
        let first = refresh_delay(expires_in);
        super::refresh_loop(
            &stopped,
            "OAuth2 token refresh",
            &live,
            first,
            RETRY_DELAY.min(first),
            || {
                let (value, expires_in) = fetch_header(&client, &config, &name)?;
                Ok((value, expires_in.map(refresh_delay)))
            },
        );
    });

    let live = first_rx
        .recv()
        .map_err(|_| ConfigError::AuthTokenRequest("the token request did not finish".into()))?
        .map_err(ConfigError::AuthTokenRequest)?;
    Ok((live, RefreshGuard { _stop: Some(stop) }))
}

/// Time from receiving a token valid for `expires_in` until it is replaced:
/// [`refresh_margin`](super::refresh_margin) before expiry.
fn refresh_delay(expires_in: Duration) -> Duration {
    (expires_in - super::refresh_margin(expires_in)).max(MIN_REFRESH_DELAY)
}

fn client() -> std::result::Result<reqwest::blocking::Client, String> {
    reqwest::blocking::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        // A token endpoint answers directly; following a redirect would
        // resend the client credentials somewhere else.
        .redirect(reqwest::redirect::Policy::none())
        .tls_backend_preconfigured(crate::connect::rustls_client_config())
        .build()
        .map_err(|e| format!("could not set up the HTTP client: {e}"))
}

/// Requests a token and returns it as the value of header `name`, with
/// its lifetime.
fn fetch_header(
    client: &reqwest::blocking::Client,
    config: &OAuth2Config,
    name: &HeaderName,
) -> std::result::Result<(HeaderValue, Option<Duration>), String> {
    let token = fetch(client, config)?;
    let value = super::header_value(AuthScheme::Static, name, token.access_token.expose())
        .ok_or_else(|| "the access token is not a valid HTTP header value".to_string())?;
    Ok((value, token.expires_in))
}

/// An issued access token.
#[derive(Debug)]
struct Token {
    access_token: Secret,
    expires_in: Option<Duration>,
}

/// The `application/x-www-form-urlencoded` body of the grant request.
fn request_body(config: &OAuth2Config) -> String {
    let mut form = form_urlencoded::Serializer::new(String::new());
    form.append_pair("grant_type", "client_credentials");
    if let Some(scope) = &config.scope {
        form.append_pair("scope", scope);
    }
    if let Some(audience) = &config.audience {
        form.append_pair("audience", audience);
    }
    form.finish()
}

fn form_encode(value: &str) -> String {
    form_urlencoded::byte_serialize(value.as_bytes()).collect()
}

/// Sends one grant request. Errors carry the HTTP status and the `error`
/// code of the response, never the secret or the token.
fn fetch(
    client: &reqwest::blocking::Client,
    config: &OAuth2Config,
) -> std::result::Result<Token, String> {
    let response = client
        .post(config.token_url.clone())
        .basic_auth(
            form_encode(&config.client_id),
            Some(form_encode(config.client_secret.expose())),
        )
        .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
        .header(ACCEPT, "application/json")
        .body(request_body(config))
        .send()
        .map_err(|e| format!("could not reach the token endpoint: {}", error_chain(e)))?;
    let status = response.status();
    let mut body = Vec::new();
    response
        .take(MAX_RESPONSE + 1)
        .read_to_end(&mut body)
        .map_err(|e| format!("could not read the token endpoint's response: {e}"))?;
    if body.len() as u64 > MAX_RESPONSE {
        return Err(format!(
            "the token endpoint's response is larger than {MAX_RESPONSE} bytes"
        ));
    }
    parse_response(status, &body)
}

/// The error and its sources, without the URL, which may carry
/// parameters the user did not mean to print.
fn error_chain(error: reqwest::Error) -> String {
    let error = error.without_url();
    let mut message = error.to_string();
    let mut source = std::error::Error::source(&error);
    while let Some(cause) = source {
        message.push_str(": ");
        message.push_str(&cause.to_string());
        source = cause.source();
    }
    message
}

/// Reads a token response (RFC 6749 sections 5.1 and 5.2).
fn parse_response(status: StatusCode, body: &[u8]) -> std::result::Result<Token, String> {
    let json = match serde_json::from_slice::<Value>(body) {
        Ok(Value::Object(json)) => Some(json),
        _ => None,
    };
    if !status.is_success() {
        let code = json.as_ref().and_then(|json| printable(json, "error"));
        return Err(match code {
            Some(code) => format!("the token endpoint answered {status}: {code}"),
            None => format!("the token endpoint answered {status}"),
        });
    }
    let json =
        json.ok_or_else(|| "the token endpoint's response is not a JSON object".to_string())?;
    let access_token = json
        .get("access_token")
        .and_then(Value::as_str)
        .filter(|token| !token.is_empty())
        .ok_or_else(|| "the token endpoint's response has no access_token".to_string())?;
    if let Some(kind) = json.get("token_type").and_then(Value::as_str)
        && !kind.eq_ignore_ascii_case("bearer")
    {
        let kind = printable(&json, "token_type").unwrap_or("another");
        return Err(format!(
            "the token endpoint issued a '{kind}' token; only Bearer tokens are supported"
        ));
    }
    let expires_in = match json.get("expires_in") {
        None | Some(Value::Null) => None,
        // Some servers send the number as a string.
        Some(value) => Some(
            value
                .as_u64()
                .or_else(|| value.as_str()?.trim().parse().ok())
                .map(Duration::from_secs)
                .ok_or_else(|| {
                    "the token endpoint's expires_in is not a number of seconds".to_string()
                })?,
        ),
    };
    Ok(Token {
        access_token: Secret::new(access_token.to_string()),
        expires_in,
    })
}

/// A short string member made only of the characters RFC 6749 allows in
/// an `error` code, so a response cannot inject text into the output.
fn printable<'a>(json: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    json.get(key).and_then(Value::as_str).filter(|value| {
        !value.is_empty()
            && value.len() <= 100
            && value
                .bytes()
                .all(|b| matches!(b, 0x20..=0x21 | 0x23..=0x5b | 0x5d..=0x7e))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    fn config() -> OAuth2Config {
        OAuth2Config {
            token_url: Url::parse("https://auth.example/oauth/token").unwrap(),
            client_id: "load test".into(),
            client_secret: Secret::new("s3cr+t:x".into()),
            scope: Some("ws.read ws.write".into()),
            audience: Some("https://api.example".into()),
        }
    }

    #[test]
    fn the_grant_request_body_is_form_encoded() {
        assert_eq!(
            request_body(&config()),
            "grant_type=client_credentials&scope=ws.read+ws.write&audience=https%3A%2F%2Fapi.example"
        );
        let bare = OAuth2Config {
            scope: None,
            audience: None,
            ..config()
        };
        assert_eq!(request_body(&bare), "grant_type=client_credentials");
        // Basic credentials are form-encoded first (RFC 6749 section 2.3.1).
        assert_eq!(form_encode("load test"), "load+test");
        assert_eq!(form_encode("s3cr+t:x"), "s3cr%2Bt%3Ax");
    }

    #[test]
    fn a_successful_response_gives_the_token_and_its_lifetime() {
        let token = parse_response(
            StatusCode::OK,
            br#"{"access_token":"at-1","token_type":"Bearer","expires_in":3600}"#,
        )
        .unwrap();
        assert_eq!(token.access_token.expose(), "at-1");
        assert_eq!(token.expires_in, Some(Duration::from_secs(3600)));

        let token = parse_response(
            StatusCode::OK,
            br#"{"access_token":"at-2","token_type":"bearer","expires_in":"120"}"#,
        )
        .unwrap();
        assert_eq!(token.expires_in, Some(Duration::from_secs(120)));

        let token = parse_response(StatusCode::OK, br#"{"access_token":"at-3"}"#).unwrap();
        assert_eq!(token.expires_in, None);
        assert!(!format!("{token:?}").contains("at-3"));
    }

    #[test]
    fn unusable_responses_are_rejected_without_the_body() {
        for (status, body, message) in [
            (
                StatusCode::UNAUTHORIZED,
                &br#"{"error":"invalid_client","error_description":"bad secret"}"#[..],
                "answered 401 Unauthorized: invalid_client",
            ),
            (
                StatusCode::BAD_REQUEST,
                br#"{"error":"bad\nline \"quoted\""}"#,
                "answered 400 Bad Request",
            ),
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                b"<html>oops at-secret</html>",
                "answered 500 Internal Server Error",
            ),
            (StatusCode::OK, b"at-secret", "not a JSON object"),
            (
                StatusCode::OK,
                br#"{"token":"at-secret"}"#,
                "no access_token",
            ),
            (
                StatusCode::OK,
                br#"{"access_token":"at-secret","token_type":"mac"}"#,
                "'mac' token; only Bearer",
            ),
            (
                StatusCode::OK,
                br#"{"access_token":"at-secret","expires_in":"soon"}"#,
                "expires_in is not a number",
            ),
        ] {
            let error = parse_response(status, body).unwrap_err();
            assert!(error.contains(message), "{error}");
            assert!(!error.contains("at-secret"), "{error}");
            assert!(!error.contains("bad secret"), "{error}");
        }
        let error =
            parse_response(StatusCode::BAD_REQUEST, br#"{"error":"bad\nline"}"#).unwrap_err();
        assert_eq!(error, "the token endpoint answered 400 Bad Request");
    }

    #[test]
    fn tokens_are_refreshed_shortly_before_they_expire() {
        assert_eq!(
            refresh_delay(Duration::from_secs(3600)),
            Duration::from_secs(3540)
        );
        assert_eq!(
            refresh_delay(Duration::from_secs(300)),
            Duration::from_secs(240)
        );
        assert_eq!(
            refresh_delay(Duration::from_secs(2)),
            Duration::from_millis(1600)
        );
        assert_eq!(refresh_delay(Duration::ZERO), MIN_REFRESH_DELAY);
    }

    #[test]
    fn options_are_validated() {
        let dir = std::env::temp_dir().join(format!("wsblast-oauth2-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let secret_file = dir.join("secret");
        std::fs::write(&secret_file, "s3cret\r\n").unwrap();
        let secret_file = secret_file.to_str().unwrap();
        let parse = |extra: &[&str]| {
            let base = ["wsblast", "ws://127.0.0.1:9001", "--auth-type", "oauth2"];
            let cli = Cli::try_parse_from(base.iter().chain(extra)).unwrap();
            config_from_cli(&cli)
        };

        let config = parse(&[
            "--auth-oauth2-token-url",
            "https://auth.example/token",
            "--auth-oauth2-client-id",
            "id",
            "--auth-oauth2-client-secret-file",
            secret_file,
            "--auth-oauth2-scope",
            "ws",
            "--auth-oauth2-audience",
            "api",
        ])
        .unwrap();
        assert_eq!(config.token_url.as_str(), "https://auth.example/token");
        assert_eq!(config.client_id, "id");
        assert_eq!(config.client_secret.expose(), "s3cret");
        assert_eq!(config.scope.as_deref(), Some("ws"));
        assert_eq!(config.audience.as_deref(), Some("api"));
        assert!(!format!("{config:?}").contains("s3cret"));

        for url in [
            "http://127.0.0.1:8080/t",
            "http://[::1]/t",
            "http://localhost/t",
        ] {
            let args = [
                "--auth-oauth2-token-url",
                url,
                "--auth-oauth2-client-id",
                "id",
                "--auth-oauth2-client-secret-file",
                secret_file,
            ];
            assert!(parse(&args).is_ok(), "{url}");
        }

        let with_url = |url: &'static str| {
            vec![
                "--auth-oauth2-token-url",
                url,
                "--auth-oauth2-client-id",
                "id",
                "--auth-oauth2-client-secret-file",
                secret_file,
            ]
        };
        for (args, message) in [
            (
                vec!["--auth-oauth2-client-id", "id"],
                "requires --auth-oauth2-token-url",
            ),
            (
                with_url("http://auth.example/token"),
                "must be an https:// URL",
            ),
            (
                with_url("ftp://auth.example/token"),
                "must be an https:// URL",
            ),
            (with_url("not a url"), "not a valid URL"),
            (
                vec![
                    "--auth-oauth2-token-url",
                    "https://auth.example/token",
                    "--auth-oauth2-client-secret-file",
                    secret_file,
                ],
                "requires --auth-oauth2-client-id",
            ),
            (
                vec![
                    "--auth-oauth2-token-url",
                    "https://auth.example/token",
                    "--auth-oauth2-client-id",
                    "id",
                    "--auth-oauth2-client-secret-file",
                    "/nonexistent/wsblast-secret",
                ],
                "could not read --auth-oauth2-client-secret-file",
            ),
        ] {
            let error = parse(&args).unwrap_err().to_string();
            assert!(error.contains(message), "{args:?}: {error}");
        }
        if std::env::var_os(CLIENT_SECRET_ENV).is_none() {
            let error = parse(&[
                "--auth-oauth2-token-url",
                "https://auth.example/token",
                "--auth-oauth2-client-id",
                "id",
            ])
            .unwrap_err()
            .to_string();
            assert!(error.contains(CLIENT_SECRET_ENV), "{error}");
        }
    }
}
