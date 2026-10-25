//! Handshake authentication: a static token or API key given on the command
//! line or printed by a command, a JWT that wsblast signs itself (`jwt`
//! feature) or an OAuth2 client-credentials token (`oauth2` feature).
//!
//! A credential is turned into one handshake header before the run starts.
//! When the credential changes during the run (`--auth-token-cmd` with
//! `--auth-token-refresh`, a JWT, or an OAuth2 token), the header lives in a
//! [`LiveAuthHeader`] that every handshake attempt reads and that a
//! background thread keeps current; otherwise it is merged into the static
//! handshake headers.

#[cfg(feature = "jwt")]
pub(crate) mod jwt;
#[cfg(feature = "oauth2")]
pub(crate) mod oauth2;

use crate::config::{AuthConfig, AuthScheme, Credential, TokenCommand};
use crate::error::ConfigError;
use http::header::{HeaderMap, HeaderName, HeaderValue};
use std::fmt;
use std::io::Read;
use std::process::{Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, PoisonError, RwLock};
use std::time::{Duration, Instant};

/// How often a running token command is checked for completion.
const POLL_INTERVAL: Duration = Duration::from_millis(5);

/// Shortest gap between two refresh failure warnings. Failures inside
/// the gap are counted and reported with the next warning.
const WARNING_GAP: Duration = Duration::from_secs(60);

/// The authentication header of a run whose credential changes while the
/// run is going. Clones share the current value.
#[derive(Clone)]
pub(crate) struct LiveAuthHeader {
    name: HeaderName,
    source: Source,
}

#[derive(Clone)]
enum Source {
    /// One value for every handshake, replaced by a refresh thread.
    Shared(Arc<RwLock<HeaderValue>>),
    /// A JWT signed for each handshake attempt.
    #[cfg(feature = "jwt")]
    PerAttempt(Arc<jwt::PerAttempt>),
}

impl LiveAuthHeader {
    fn new(name: HeaderName, value: HeaderValue) -> Self {
        Self {
            name,
            source: Source::Shared(Arc::new(RwLock::new(value))),
        }
    }

    /// Sets the header on a handshake request, replacing any value it had.
    /// A shared value is read under the lock only to clone it, which shares
    /// its bytes instead of copying them.
    pub(crate) fn apply(&self, headers: &mut HeaderMap) {
        let value = match &self.source {
            Source::Shared(value) => value.read().unwrap_or_else(PoisonError::into_inner).clone(),
            #[cfg(feature = "jwt")]
            Source::PerAttempt(signer) => match signer.header_value() {
                Some(value) => value,
                None => {
                    headers.remove(&self.name);
                    return;
                }
            },
        };
        headers.insert(self.name.clone(), value);
    }

    fn set(&self, value: HeaderValue) {
        match &self.source {
            Source::Shared(shared) => {
                *shared.write().unwrap_or_else(PoisonError::into_inner) = value;
            }
            #[cfg(feature = "jwt")]
            Source::PerAttempt(_) => {}
        }
    }
}

impl fmt::Debug for LiveAuthHeader {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LiveAuthHeader")
            .field("name", &self.name)
            .field("value", &"[redacted]")
            .finish()
    }
}

/// Stops the background token refresh, if there is one, when dropped.
#[derive(Debug)]
pub(crate) struct RefreshGuard {
    // Dropping the sender wakes the refresh thread, which then exits.
    _stop: Option<mpsc::Sender<()>>,
}

impl RefreshGuard {
    /// A guard for a live header that no thread refreshes.
    #[cfg(feature = "jwt")]
    fn none() -> Self {
        Self { _stop: None }
    }
}

/// Resolves `auth` into its handshake header before the run starts.
///
/// A static credential, or a command credential without refresh, is
/// merged into `headers`. A credential that changes during the run is
/// removed from `headers` and returned as a [`LiveAuthHeader`] together
/// with the guard of the thread that refreshes it.
pub(crate) fn prepare_auth(
    auth: &AuthConfig,
    headers: &mut HeaderMap,
) -> Result<Option<(LiveAuthHeader, RefreshGuard)>, ConfigError> {
    let name = HeaderName::from_bytes(auth.header.as_bytes()).map_err(|e| {
        ConfigError::InvalidAuth(format!(
            "invalid authentication header '{}': {e}",
            auth.header
        ))
    })?;
    let value = match &auth.credential {
        Credential::Value(token) => {
            header_value(auth.scheme, &name, token.expose()).ok_or_else(|| {
                ConfigError::InvalidAuth(
                    "the credential is not a valid HTTP header value".to_string(),
                )
            })?
        }
        Credential::Command(command) => {
            fetch_header(auth.scheme, &name, command).map_err(ConfigError::AuthTokenCommand)?
        }
        #[cfg(feature = "jwt")]
        Credential::Jwt(config) => {
            headers.remove(&name);
            return jwt::prepare(config, name).map(Some);
        }
        #[cfg(feature = "oauth2")]
        Credential::OAuth2(config) => {
            headers.remove(&name);
            return oauth2::prepare(config, name).map(Some);
        }
    };

    if let Credential::Command(command) = &auth.credential
        && let Some(every) = command.refresh
    {
        headers.remove(&name);
        let live = LiveAuthHeader::new(name.clone(), value);
        let scheme = auth.scheme;
        let command = command.clone();
        let guard = spawn_refresh_loop(
            "--auth-token-cmd refresh",
            live.clone(),
            every,
            every,
            move || Ok((fetch_header(scheme, &name, &command)?, Some(every))),
        );
        return Ok(Some((live, guard)));
    }
    headers.insert(name, value);
    Ok(None)
}

/// Builds the header value for `token`: a static token sent in
/// `Authorization` gets a `Bearer ` prefix unless it has one. Returns
/// `None` when the result is not a valid header value. The value is
/// marked sensitive, so its `Debug` output is redacted.
pub(crate) fn header_value(
    scheme: AuthScheme,
    name: &HeaderName,
    token: &str,
) -> Option<HeaderValue> {
    let bearer = scheme == AuthScheme::Static
        && *name == http::header::AUTHORIZATION
        && !token
            .get(..7)
            .is_some_and(|p| p.eq_ignore_ascii_case("bearer "));
    let mut value = if bearer {
        HeaderValue::from_str(&format!("Bearer {token}"))
    } else {
        HeaderValue::from_str(token)
    }
    .ok()?;
    value.set_sensitive(true);
    Some(value)
}

/// Runs the token command and turns its output into the header value.
fn fetch_header(
    scheme: AuthScheme,
    name: &HeaderName,
    command: &TokenCommand,
) -> Result<HeaderValue, String> {
    let token = run_token_command(&command.command, command.timeout)?;
    header_value(scheme, name, &token)
        .ok_or_else(|| "its output is not a valid HTTP header value".to_string())
}

/// Runs `command` through the platform shell (`sh -c` on Unix, `cmd /C`
/// on Windows) and returns its standard output with surrounding
/// whitespace removed.
///
/// Fails when the command cannot be started, does not exit within
/// `timeout` (it is then killed), exits unsuccessfully, or prints
/// nothing. Error messages carry the exit status and the first line of
/// standard error, never the output itself.
pub(crate) fn run_token_command(command: &str, timeout: Duration) -> Result<String, String> {
    let deadline = Instant::now() + timeout;
    let mut child = shell(command)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("could not start the command: {e}"))?;
    // The pipes are drained on their own threads so a command that
    // writes more than a pipe buffer holds cannot block.
    let stdout = drain(child.stdout.take());
    let stderr = drain(child.stderr.take());

    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("the command did not finish within {timeout:?}"));
            }
            Ok(None) => std::thread::sleep(POLL_INTERVAL),
            Err(e) => return Err(format!("could not wait for the command: {e}")),
        }
    };

    // A process the command left running in the background can keep the
    // pipes open after the command exits; wait for them only until the
    // deadline.
    let remaining = deadline.saturating_duration_since(Instant::now());
    let stderr = stderr.recv_timeout(remaining).unwrap_or_default();
    if !status.success() {
        return Err(failure(status, &stderr));
    }
    let remaining = deadline.saturating_duration_since(Instant::now());
    let stdout = stdout
        .recv_timeout(remaining)
        .map_err(|_| format!("the command's output was not closed within {timeout:?}"))?;
    let token = String::from_utf8(stdout)
        .map_err(|_| "its output is not a valid HTTP header value".to_string())?
        .trim()
        .to_string();
    if token.is_empty() {
        return Err(format!("the command printed no token ({status})"));
    }
    Ok(token)
}

#[cfg(unix)]
fn shell(command: &str) -> Command {
    let mut shell = Command::new("sh");
    shell.arg("-c").arg(command);
    shell
}

#[cfg(windows)]
fn shell(command: &str) -> Command {
    use std::os::windows::process::CommandExt;
    let mut shell = Command::new("cmd");
    // cmd parses its own command line, so pass the command unquoted.
    shell.arg("/C").raw_arg(command);
    shell
}

fn drain(pipe: Option<impl Read + Send + 'static>) -> mpsc::Receiver<Vec<u8>> {
    let (tx, rx) = mpsc::channel();
    if let Some(mut pipe) = pipe {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = pipe.read_to_end(&mut buf);
            let _ = tx.send(buf);
        });
    }
    rx
}

fn failure(status: ExitStatus, stderr: &[u8]) -> String {
    let stderr = String::from_utf8_lossy(stderr);
    match stderr.lines().map(str::trim).find(|line| !line.is_empty()) {
        Some(line) => format!("the command failed ({status}): {line}"),
        None => format!("the command failed ({status})"),
    }
}

/// What one refresh produced: the new header value and the time until
/// the next refresh, or `None` when the value no longer needs refreshing.
type Refreshed = Result<(HeaderValue, Option<Duration>), String>;

/// Runs `refresh` on a plain thread, first after `first` and then after
/// the delay each refresh returns, so a slow refresh never occupies the
/// runtime and never delays process exit. A failed refresh keeps the
/// previous value, warns (rate limited), and is retried after `retry`.
fn spawn_refresh_loop(
    what: &'static str,
    live: LiveAuthHeader,
    first: Duration,
    retry: Duration,
    refresh: impl FnMut() -> Refreshed + Send + 'static,
) -> RefreshGuard {
    let (stop, stopped) = mpsc::channel::<()>();
    std::thread::spawn(move || refresh_loop(&stopped, what, &live, first, retry, refresh));
    RefreshGuard { _stop: Some(stop) }
}

/// The body of a refresh thread; returns when `stopped` is signalled or
/// disconnected. Each wait starts when the previous refresh finished.
fn refresh_loop(
    stopped: &mpsc::Receiver<()>,
    what: &str,
    live: &LiveAuthHeader,
    first: Duration,
    retry: Duration,
    mut refresh: impl FnMut() -> Refreshed,
) {
    let mut warnings = WarningLimiter::new(WARNING_GAP);
    let mut wait = first;
    loop {
        match stopped.recv_timeout(wait) {
            Err(RecvTimeoutError::Timeout) => {}
            _ => return,
        }
        match refresh() {
            Ok((value, next)) => {
                live.set(value);
                warnings.succeeded();
                match next {
                    Some(next) => wait = next,
                    None => return,
                }
            }
            Err(err) => {
                if let Some(suppressed) = warnings.failed(Instant::now()) {
                    warn(
                        &format!("{what} failed: {err}; new handshakes keep the previous token"),
                        suppressed,
                    );
                }
                wait = retry;
            }
        }
    }
}

/// How long before a credential expires it is replaced: a fifth of its
/// lifetime, at most one minute.
#[cfg(any(feature = "jwt", feature = "oauth2"))]
fn refresh_margin(lifetime: Duration) -> Duration {
    (lifetime / 5).min(Duration::from_secs(60))
}

/// Prints a warning to stderr, with the count of earlier ones not shown.
fn warn(message: &str, suppressed: u64) {
    use colored::Colorize;
    let earlier = if suppressed > 0 {
        format!(" ({suppressed} earlier failures not shown)")
    } else {
        String::new()
    };
    eprintln!("{} {message}{earlier}", "Warning:".bold().yellow());
}

/// Lets one warning through per failure, but at most one per `gap`,
/// counting the failures it held back.
pub(crate) struct WarningLimiter {
    gap: Duration,
    last: Option<Instant>,
    suppressed: u64,
}

impl WarningLimiter {
    pub(crate) fn new(gap: Duration) -> Self {
        Self {
            gap,
            last: None,
            suppressed: 0,
        }
    }

    /// Records a failure at `now`. Returns the number of failures held
    /// back since the last warning when this one should be printed.
    pub(crate) fn failed(&mut self, now: Instant) -> Option<u64> {
        if self
            .last
            .is_some_and(|last| now.duration_since(last) < self.gap)
        {
            self.suppressed += 1;
            return None;
        }
        self.last = Some(now);
        Some(std::mem::take(&mut self.suppressed))
    }

    /// Records a successful refresh. Held-back failures are dropped, as
    /// the token is current again.
    pub(crate) fn succeeded(&mut self) {
        self.suppressed = 0;
    }
}

/// What a distributed run needs of a refreshed header: the coordinator
/// reads each new value and sends it to its agents, and an agent keeps the
/// value it receives in a header of its own, which its handshakes read as
/// they would on a single machine.
#[cfg(feature = "distributed")]
impl LiveAuthHeader {
    /// A header holding `value`, updated only through
    /// [`replace`](Self::replace).
    pub(crate) fn received(name: HeaderName, value: HeaderValue) -> Self {
        Self::new(name, value)
    }

    /// The header name.
    pub(crate) fn name(&self) -> &HeaderName {
        &self.name
    }

    /// The current value. A header signed per attempt has no shared value,
    /// so this signs one; the coordinator rejects that mode with agents.
    pub(crate) fn current(&self) -> HeaderValue {
        match &self.source {
            Source::Shared(value) => value.read().unwrap_or_else(PoisonError::into_inner).clone(),
            #[cfg(feature = "jwt")]
            Source::PerAttempt(signer) => signer
                .header_value()
                .unwrap_or_else(|| HeaderValue::from_static("")),
        }
    }

    /// Replaces the value for every later handshake.
    pub(crate) fn replace(&self, value: HeaderValue) {
        self.set(value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AuthConfig, AuthScheme, Credential, Secret, TokenCommand};
    use http::HeaderMap;
    use http::header::{AUTHORIZATION, HeaderName};
    use std::time::{Duration, Instant};

    const TIMEOUT: Duration = Duration::from_secs(10);

    fn resolve(scheme: AuthScheme, header: &str, token: &str) -> HeaderMap {
        let auth = AuthConfig {
            scheme,
            header: header.to_string(),
            credential: Credential::Value(Secret::new(token.to_string())),
        };
        let mut headers = HeaderMap::new();
        assert!(prepare_auth(&auth, &mut headers).unwrap().is_none());
        headers
    }

    #[test]
    fn static_authorization_gets_bearer_prefix() {
        let headers = resolve(AuthScheme::Static, "Authorization", "token-123");
        assert_eq!(headers[AUTHORIZATION], "Bearer token-123");
    }

    #[test]
    fn static_bearer_token_is_not_prefixed_twice() {
        let headers = resolve(AuthScheme::Static, "Authorization", "bearer token-123");
        assert_eq!(headers[AUTHORIZATION], "bearer token-123");
    }

    #[test]
    fn static_custom_header_preserves_raw_token() {
        let headers = resolve(AuthScheme::Static, "X-Session", "token-123");
        assert_eq!(headers["x-session"], "token-123");
    }

    #[test]
    fn api_key_uses_configured_header() {
        let headers = resolve(AuthScheme::ApiKey, "X-Api-Key", "secret");
        assert_eq!(headers["x-api-key"], "secret");
        let headers = resolve(AuthScheme::ApiKey, "Authorization", "secret");
        assert_eq!(headers[AUTHORIZATION], "secret");
    }

    #[test]
    fn invalid_header_name_is_rejected() {
        let auth = AuthConfig {
            scheme: AuthScheme::Static,
            header: "bad header".to_string(),
            credential: Credential::Value(Secret::new("token".to_string())),
        };
        let error = prepare_auth(&auth, &mut HeaderMap::new()).unwrap_err();
        assert!(error.to_string().contains("invalid authentication header"));
    }

    #[test]
    fn header_values_are_redacted_in_debug_output() {
        let headers = resolve(AuthScheme::Static, "Authorization", "token-123");
        assert!(!format!("{headers:?}").contains("token-123"));
        let auth = AuthConfig {
            scheme: AuthScheme::Static,
            header: "Authorization".to_string(),
            credential: Credential::Value(Secret::new("token-123".to_string())),
        };
        assert!(!format!("{auth:?}").contains("token-123"));
        let value = header_value(AuthScheme::Static, &AUTHORIZATION, "token-123").unwrap();
        let live = LiveAuthHeader::new(AUTHORIZATION, value);
        assert!(!format!("{live:?}").contains("token-123"));
    }

    #[test]
    fn command_output_is_the_token() {
        assert_eq!(
            run_token_command("echo token-1", TIMEOUT).unwrap(),
            "token-1"
        );
    }

    #[cfg(unix)]
    #[test]
    fn surrounding_whitespace_and_trailing_newlines_are_trimmed() {
        let token = run_token_command(r"printf '  token-1 \n\n'", TIMEOUT).unwrap();
        assert_eq!(token, "token-1");
    }

    #[cfg(windows)]
    #[test]
    fn surrounding_whitespace_and_trailing_newlines_are_trimmed() {
        // cmd's echo ends the line with \r\n.
        let token = run_token_command("echo token-1 ", TIMEOUT).unwrap();
        assert_eq!(token, "token-1");
    }

    #[test]
    fn non_zero_exit_reports_the_status() {
        let error = run_token_command("exit 3", TIMEOUT).unwrap_err();
        assert!(error.contains("failed"), "{error}");
        assert!(error.contains('3'), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn non_zero_exit_reports_the_first_stderr_line_and_not_stdout() {
        let error = run_token_command(
            "echo secret-output; echo 'first line' >&2; echo 'second line' >&2; exit 3",
            TIMEOUT,
        )
        .unwrap_err();
        assert!(error.contains("exit status: 3"), "{error}");
        assert!(error.contains("first line"), "{error}");
        assert!(!error.contains("second line"), "{error}");
        assert!(!error.contains("secret-output"), "{error}");
    }

    #[test]
    fn empty_output_is_rejected() {
        let error = run_token_command("exit 0", TIMEOUT).unwrap_err();
        assert!(error.contains("printed no token"), "{error}");
    }

    #[test]
    fn a_command_that_runs_too_long_is_killed() {
        #[cfg(unix)]
        let command = "sleep 10";
        #[cfg(windows)]
        let command = "ping -n 11 127.0.0.1 >NUL";
        let start = Instant::now();
        let error = run_token_command(command, Duration::from_millis(300)).unwrap_err();
        assert!(error.contains("did not finish within 300ms"), "{error}");
        assert!(start.elapsed() < Duration::from_secs(5));
    }

    #[cfg(unix)]
    #[test]
    fn output_that_is_not_a_header_value_is_rejected_without_echoing_it() {
        let auth = AuthConfig {
            scheme: AuthScheme::Static,
            header: "Authorization".to_string(),
            credential: Credential::Command(TokenCommand {
                command: r"printf 'secret\001token'".to_string(),
                timeout: TIMEOUT,
                refresh: None,
            }),
        };
        let error = prepare_auth(&auth, &mut HeaderMap::new())
            .unwrap_err()
            .to_string();
        assert!(error.contains("not a valid HTTP header value"), "{error}");
        assert!(!error.contains("secret"), "{error}");
    }

    #[test]
    fn command_token_without_refresh_becomes_a_static_header() {
        let auth = AuthConfig {
            scheme: AuthScheme::Static,
            header: "Authorization".to_string(),
            credential: Credential::Command(TokenCommand {
                command: "echo token-1".to_string(),
                timeout: TIMEOUT,
                refresh: None,
            }),
        };
        let mut headers = HeaderMap::new();
        assert!(prepare_auth(&auth, &mut headers).unwrap().is_none());
        assert_eq!(headers[AUTHORIZATION], "Bearer token-1");
    }

    #[test]
    fn refreshed_token_replaces_the_static_header() {
        let auth = AuthConfig {
            scheme: AuthScheme::ApiKey,
            header: "X-Api-Key".to_string(),
            credential: Credential::Command(TokenCommand {
                command: "echo key-1".to_string(),
                timeout: TIMEOUT,
                refresh: Some(Duration::from_secs(3600)),
            }),
        };
        let name = HeaderName::from_static("x-api-key");
        let mut headers = HeaderMap::new();
        headers.insert(name.clone(), "from -H".parse().unwrap());
        let (live, _guard) = prepare_auth(&auth, &mut headers).unwrap().unwrap();
        assert!(headers.is_empty());

        let mut request = HeaderMap::new();
        live.apply(&mut request);
        assert_eq!(request[&name], "key-1");
        live.set("key-2".parse().unwrap());
        live.apply(&mut request);
        assert_eq!(request[&name], "key-2");
        assert_eq!(request.len(), 1);
    }

    #[test]
    fn refresh_warnings_are_rate_limited() {
        let gap = Duration::from_secs(60);
        let mut limiter = WarningLimiter::new(gap);
        let t0 = Instant::now();
        assert_eq!(limiter.failed(t0), Some(0));
        assert_eq!(limiter.failed(t0 + Duration::from_secs(10)), None);
        assert_eq!(limiter.failed(t0 + Duration::from_secs(20)), None);
        assert_eq!(limiter.failed(t0 + gap), Some(2));
        limiter.failed(t0 + gap + Duration::from_secs(1));
        limiter.succeeded();
        assert_eq!(limiter.failed(t0 + gap * 2), Some(0));
    }
}
