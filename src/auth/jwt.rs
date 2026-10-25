//! `--auth-type jwt`: JSON Web Tokens that wsblast signs itself (RFC 7519,
//! JWS compact serialization from RFC 7515), with ring as the only crypto
//! implementation.
//!
//! Every token carries the `--auth-jwt-claims` object plus `iat`, `nbf`,
//! `exp` and a random `jti`. By default one token is shared by every
//! handshake and re-signed by a background thread before it expires; with
//! `--auth-jwt-per-connection` each handshake attempt signs its own.

use super::{LiveAuthHeader, RefreshGuard, Source, WARNING_GAP, WarningLimiter};
use crate::cli::{Cli, JwtAlg};
use crate::config::AuthScheme;
use crate::error::{ConfigError, Result, WsBlastError};
use http::header::{HeaderName, HeaderValue};
use ring::hmac;
use ring::rand::{SecureRandom, SystemRandom};
use ring::signature::{
    ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair, RSA_PKCS1_SHA256, RsaKeyPair,
};
use serde_json::{Map, Value};
use std::fmt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Environment variable read for the key file when `--auth-jwt-key` is
/// not given.
pub(crate) const KEY_FILE_ENV: &str = "WSBLAST_JWT_KEY_FILE";

/// Token lifetime without `--auth-jwt-ttl`.
const DEFAULT_TTL: Duration = Duration::from_secs(5 * 60);

/// Shortest `--auth-jwt-ttl`. Claims are whole seconds, so a shorter
/// lifetime would leave no room to re-sign before expiry.
const MIN_TTL: Duration = Duration::from_secs(2);

/// Shortest wait between two re-signings.
const MIN_REFRESH_DELAY: Duration = Duration::from_millis(100);

/// Wait before signing again after a failed re-signing.
const RETRY_DELAY: Duration = Duration::from_secs(1);

/// Claims wsblast sets on every token, which `--auth-jwt-claims` must not.
const REGISTERED: [&str; 4] = ["iat", "nbf", "exp", "jti"];

/// Random bytes in each `jti`.
const JTI_BYTES: usize = 16;

/// The validated `--auth-jwt-*` options.
#[derive(Debug, Clone)]
pub(crate) struct JwtConfig {
    pub(crate) alg: JwtAlg,
    /// Read when the run starts, never stored in the configuration.
    pub(crate) key_file: PathBuf,
    /// Added to every token.
    pub(crate) claims: Map<String, Value>,
    pub(crate) ttl: Duration,
    pub(crate) per_connection: bool,
}

/// Validates the `--auth-jwt-*` options of `--auth-type jwt`.
pub(crate) fn config_from_cli(cli: &Cli) -> Result<JwtConfig> {
    let invalid = |message: String| WsBlastError::from(ConfigError::InvalidAuth(message));
    let key_file = cli
        .auth_jwt_key
        .clone()
        .or_else(|| {
            std::env::var_os(KEY_FILE_ENV)
                .filter(|path| !path.is_empty())
                .map(PathBuf::from)
        })
        .ok_or_else(|| {
            invalid(format!(
                "--auth-type jwt requires --auth-jwt-key FILE or {KEY_FILE_ENV}"
            ))
        })?;

    let claims = match &cli.auth_jwt_claims {
        None => Map::new(),
        Some(json) => match serde_json::from_str::<Value>(json) {
            Ok(Value::Object(claims)) => claims,
            Ok(_) => return Err(invalid("--auth-jwt-claims must be a JSON object".into())),
            Err(e) => return Err(invalid(format!("--auth-jwt-claims is not valid JSON: {e}"))),
        },
    };
    if let Some(claim) = REGISTERED.iter().find(|claim| claims.contains_key(**claim)) {
        return Err(invalid(format!(
            "--auth-jwt-claims must not set '{claim}': wsblast sets iat, nbf and exp from \
             --auth-jwt-ttl and a unique jti on every token"
        )));
    }

    let ttl = cli
        .auth_jwt_ttl
        .as_deref()
        .map_or(Ok(DEFAULT_TTL), crate::config::parse_human_duration)?;
    if ttl < MIN_TTL {
        return Err(invalid(format!(
            "--auth-jwt-ttl must be at least {}s",
            MIN_TTL.as_secs()
        )));
    }

    Ok(JwtConfig {
        alg: cli.auth_jwt_alg.unwrap_or(JwtAlg::Hs256),
        key_file,
        claims,
        ttl,
        per_connection: cli.auth_jwt_per_connection,
    })
}

/// Loads the key and signs the first token, so a bad key fails before the
/// run. Returns the live header and, for a shared token, the guard of the
/// thread that re-signs it.
pub(super) fn prepare(
    config: &JwtConfig,
    name: HeaderName,
) -> std::result::Result<(LiveAuthHeader, RefreshGuard), ConfigError> {
    let signer = Signer::load(config).map_err(ConfigError::InvalidAuth)?;
    let (value, expires) = signer
        .header_value(&name, unix_now())
        .map_err(ConfigError::InvalidAuth)?;

    if config.per_connection {
        let per_attempt = PerAttempt {
            signer,
            name: name.clone(),
            warnings: Mutex::new(WarningLimiter::new(WARNING_GAP)),
        };
        let live = LiveAuthHeader {
            name,
            source: Source::PerAttempt(Arc::new(per_attempt)),
        };
        return Ok((live, RefreshGuard::none()));
    }

    let live = LiveAuthHeader::new(name.clone(), value);
    let ttl = signer.ttl;
    let first = refresh_delay(expires, ttl, SystemTime::now());
    let guard = super::spawn_refresh_loop(
        "JWT re-signing",
        live.clone(),
        first,
        RETRY_DELAY,
        move || {
            let (value, expires) = signer.header_value(&name, unix_now())?;
            Ok((value, Some(refresh_delay(expires, ttl, SystemTime::now()))))
        },
    );
    Ok((live, guard))
}

/// Time from `now` until a token that expires at `expires` (Unix seconds)
/// is replaced: [`refresh_margin`](super::refresh_margin) before expiry.
fn refresh_delay(expires: u64, ttl: Duration, now: SystemTime) -> Duration {
    let replace_at = UNIX_EPOCH + Duration::from_secs(expires) - super::refresh_margin(ttl);
    replace_at
        .duration_since(now)
        .unwrap_or_default()
        .max(MIN_REFRESH_DELAY)
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Signs a token for every handshake attempt (`--auth-jwt-per-connection`).
pub(crate) struct PerAttempt {
    signer: Signer,
    name: HeaderName,
    warnings: Mutex<WarningLimiter>,
}

impl PerAttempt {
    /// A freshly signed header value, or `None` (after a rate-limited
    /// warning) when signing failed.
    pub(super) fn header_value(&self) -> Option<HeaderValue> {
        match self.signer.header_value(&self.name, unix_now()) {
            Ok((value, _)) => Some(value),
            Err(err) => {
                let shown = self
                    .warnings
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .failed(Instant::now());
                if let Some(suppressed) = shown {
                    super::warn(
                        &format!("JWT signing failed: {err}; the handshake is sent without it"),
                        suppressed,
                    );
                }
                None
            }
        }
    }
}

/// A loaded signing key with the parts every token shares.
pub(crate) struct Signer {
    key: SigningKey,
    /// The encoded JOSE header.
    header: String,
    claims: Map<String, Value>,
    ttl: Duration,
    rng: SystemRandom,
}

enum SigningKey {
    Hmac(hmac::Key),
    Rsa(RsaKeyPair),
    Ecdsa(EcdsaKeyPair),
}

impl fmt::Debug for Signer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Signer")
            .field("header", &self.header)
            .field("key", &"[redacted]")
            .finish_non_exhaustive()
    }
}

/// A signed token and its `exp` claim.
pub(crate) struct Token {
    pub(crate) token: String,
    pub(crate) expires: u64,
}

impl Signer {
    /// Reads and parses `config.key_file`. Errors name the file but never
    /// show its contents.
    pub(crate) fn load(config: &JwtConfig) -> std::result::Result<Self, String> {
        let path = &config.key_file;
        let bytes = std::fs::read(path)
            .map_err(|e| format!("could not read --auth-jwt-key {}: {e}", path.display()))?;
        Self::new(config.alg, &bytes, config.claims.clone(), config.ttl)
            .map_err(|e| format!("--auth-jwt-key {}: {e}", path.display()))
    }

    fn new(
        alg: JwtAlg,
        key: &[u8],
        claims: Map<String, Value>,
        ttl: Duration,
    ) -> std::result::Result<Self, String> {
        let rng = SystemRandom::new();
        let key = match alg {
            JwtAlg::Hs256 => SigningKey::Hmac(hmac_key(hmac::HMAC_SHA256, key)?),
            JwtAlg::Hs384 => SigningKey::Hmac(hmac_key(hmac::HMAC_SHA384, key)?),
            JwtAlg::Hs512 => SigningKey::Hmac(hmac_key(hmac::HMAC_SHA512, key)?),
            JwtAlg::Rs256 => SigningKey::Rsa(rsa_key(key)?),
            JwtAlg::Es256 => SigningKey::Ecdsa(ecdsa_key(key, &rng)?),
        };
        let header = format!(r#"{{"alg":"{}","typ":"JWT"}}"#, alg.name());
        Ok(Self {
            key,
            header: crate::base64::encode_url(header.as_bytes()),
            claims,
            ttl,
            rng,
        })
    }

    /// Signs a token issued at `now` (Unix seconds) with a new `jti`.
    pub(crate) fn sign_at(&self, now: u64) -> std::result::Result<Token, String> {
        let mut jti = [0u8; JTI_BYTES];
        self.rng
            .fill(&mut jti)
            .map_err(|_| "the system random number generator failed".to_string())?;
        let expires = now.saturating_add(self.ttl.as_secs());
        let mut claims = self.claims.clone();
        claims.insert("iat".into(), now.into());
        claims.insert("nbf".into(), now.into());
        claims.insert("exp".into(), expires.into());
        claims.insert("jti".into(), crate::base64::encode_url(&jti).into());
        let payload = Value::Object(claims).to_string();

        let mut token = format!(
            "{}.{}",
            self.header,
            crate::base64::encode_url(payload.as_bytes())
        );
        let input = token.as_bytes();
        let failed = |_| "signing failed".to_string();
        let signature = match &self.key {
            SigningKey::Hmac(key) => hmac::sign(key, input).as_ref().to_vec(),
            SigningKey::Rsa(key) => {
                let mut signature = vec![0; key.public().modulus_len()];
                key.sign(&RSA_PKCS1_SHA256, &self.rng, input, &mut signature)
                    .map_err(failed)?;
                signature
            }
            SigningKey::Ecdsa(key) => key
                .sign(&self.rng, input)
                .map_err(failed)?
                .as_ref()
                .to_vec(),
        };
        token.push('.');
        token.push_str(&crate::base64::encode_url(&signature));
        Ok(Token { token, expires })
    }

    /// Signs a token issued at `now` and returns it as the value of
    /// header `name`, with its `exp`.
    fn header_value(
        &self,
        name: &HeaderName,
        now: u64,
    ) -> std::result::Result<(HeaderValue, u64), String> {
        let Token { token, expires } = self.sign_at(now)?;
        let value = super::header_value(AuthScheme::Static, name, &token)
            .ok_or_else(|| "the token is not a valid HTTP header value".to_string())?;
        Ok((value, expires))
    }
}

/// An HMAC key from the file's bytes, without one trailing newline, so a
/// secret written with `echo` works.
fn hmac_key(alg: hmac::Algorithm, bytes: &[u8]) -> std::result::Result<hmac::Key, String> {
    let secret = bytes
        .strip_suffix(b"\r\n")
        .or_else(|| bytes.strip_suffix(b"\n"))
        .unwrap_or(bytes);
    if secret.is_empty() {
        return Err("the HMAC secret is empty".into());
    }
    Ok(hmac::Key::new(alg, secret))
}

fn rsa_key(bytes: &[u8]) -> std::result::Result<RsaKeyPair, String> {
    let (label, der) = pem_block(bytes)?;
    match label {
        "PRIVATE KEY" => RsaKeyPair::from_pkcs8(&der),
        "RSA PRIVATE KEY" => RsaKeyPair::from_der(&der),
        _ => {
            return Err(unexpected_label(
                label,
                "RS256",
                "PRIVATE KEY or RSA PRIVATE KEY",
            ));
        }
    }
    .map_err(|e| format!("not a usable RSA private key ({e}); RS256 needs 2048 to 8192 bits"))
}

fn ecdsa_key(bytes: &[u8], rng: &SystemRandom) -> std::result::Result<EcdsaKeyPair, String> {
    let (label, der) = pem_block(bytes)?;
    match label {
        "PRIVATE KEY" => EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &der, rng)
            .map_err(|e| format!("not a usable P-256 private key ({e})")),
        "EC PRIVATE KEY" => Err(
            "ES256 keys must be PKCS#8 ('BEGIN PRIVATE KEY'); convert this \
             SEC1 key with `openssl pkcs8 -topk8 -nocrypt -in KEY -out KEY.pkcs8.pem`"
                .into(),
        ),
        _ => Err(unexpected_label(label, "ES256", "PRIVATE KEY")),
    }
}

fn unexpected_label(label: &str, alg: &str, expected: &str) -> String {
    if label.contains("ENCRYPTED") {
        return "encrypted private keys are not supported; decrypt the key first".into();
    }
    format!("{alg} needs a PEM '{expected}' block, found '{label}'")
}

/// The label and DER contents of the first PEM block in `bytes` (RFC 7468).
fn pem_block(bytes: &[u8]) -> std::result::Result<(&str, Vec<u8>), String> {
    let not_pem = || "not a PEM file".to_string();
    let text = std::str::from_utf8(bytes).map_err(|_| not_pem())?;
    let mut lines = text.lines().map(str::trim);
    let label = lines
        .by_ref()
        .find_map(|line| line.strip_prefix("-----BEGIN ")?.strip_suffix("-----"))
        .ok_or_else(not_pem)?;
    let end = format!("-----END {label}-----");
    let mut body = String::new();
    for line in lines {
        if line == end {
            let der = crate::base64::decode(&body)
                .ok_or_else(|| "the PEM block is not valid base64".to_string())?;
            return Ok((label, der));
        }
        if line.contains(':') {
            // RFC 1421 headers, used only by encrypted legacy PEM keys.
            return Err("encrypted private keys are not supported; decrypt the key first".into());
        }
        body.push_str(line);
    }
    Err(format!("the PEM block has no '{end}' line"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::Cli;
    use clap::Parser;
    use http::HeaderMap;
    use http::header::AUTHORIZATION;
    use ring::signature::{
        ECDSA_P256_SHA256_FIXED, KeyPair, RSA_PKCS1_2048_8192_SHA256, UnparsedPublicKey,
    };
    use std::collections::HashSet;

    const RSA_PKCS8: &str = include_str!("../../tests/fixtures/jwt/rs256-pkcs8.pem");
    const RSA_PKCS1: &str = include_str!("../../tests/fixtures/jwt/rs256-pkcs1.pem");

    fn signer(alg: JwtAlg, key: &[u8], claims: Value, ttl: Duration) -> Signer {
        let Value::Object(claims) = claims else {
            panic!("claims must be an object")
        };
        Signer::new(alg, key, claims, ttl).unwrap()
    }

    fn b64url_decode(input: &str) -> Vec<u8> {
        let mut standard: String = input
            .chars()
            .map(|c| match c {
                '-' => '+',
                '_' => '/',
                c => c,
            })
            .collect();
        while !standard.len().is_multiple_of(4) {
            standard.push('=');
        }
        crate::base64::decode(&standard).expect("valid base64url")
    }

    /// PEM-encodes DER, for keys generated in a test.
    fn pem(label: &str, der: &[u8]) -> String {
        let mut body: String = crate::base64::encode_url(der)
            .chars()
            .map(|c| match c {
                '-' => '+',
                '_' => '/',
                c => c,
            })
            .collect();
        while !body.len().is_multiple_of(4) {
            body.push('=');
        }
        let lines: Vec<&str> = body
            .as_bytes()
            .chunks(64)
            .map(|line| std::str::from_utf8(line).unwrap())
            .collect();
        format!(
            "-----BEGIN {label}-----\n{}\n-----END {label}-----\n",
            lines.join("\n")
        )
    }

    /// Splits a token, checks its header and returns the signing input,
    /// the decoded claims and the signature.
    fn parts(token: &str, alg: &str) -> (String, Map<String, Value>, Vec<u8>) {
        let mut parts = token.split('.');
        let (header, payload, signature) = (
            parts.next().unwrap(),
            parts.next().unwrap(),
            parts.next().unwrap(),
        );
        assert!(parts.next().is_none(), "{token}");
        for part in [header, payload, signature] {
            assert!(
                part.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
                "not unpadded base64url: {part}"
            );
        }
        let header: Value = serde_json::from_slice(&b64url_decode(header)).unwrap();
        assert_eq!(header, serde_json::json!({"alg": alg, "typ": "JWT"}));
        let Value::Object(claims) = serde_json::from_slice(&b64url_decode(payload)).unwrap() else {
            panic!("claims are not an object");
        };
        let input = token.rsplit_once('.').unwrap().0.to_string();
        (input, claims, b64url_decode(signature))
    }

    #[test]
    fn hmac_tokens_verify_for_every_hash() {
        let secret = b"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        for (alg, verify_alg, name, len) in [
            (JwtAlg::Hs256, hmac::HMAC_SHA256, "HS256", 32),
            (JwtAlg::Hs384, hmac::HMAC_SHA384, "HS384", 48),
            (JwtAlg::Hs512, hmac::HMAC_SHA512, "HS512", 64),
        ] {
            let token = signer(alg, secret, serde_json::json!({}), DEFAULT_TTL)
                .sign_at(1_700_000_000)
                .unwrap()
                .token;
            let (input, _, signature) = parts(&token, name);
            assert_eq!(signature.len(), len, "{name}");
            let key = hmac::Key::new(verify_alg, secret);
            hmac::verify(&key, input.as_bytes(), &signature).unwrap();
            let wrong = hmac::Key::new(verify_alg, b"another secret");
            assert!(hmac::verify(&wrong, input.as_bytes(), &signature).is_err());
        }
    }

    #[test]
    fn hmac_secret_loses_one_trailing_newline_only() {
        let a = signer(
            JwtAlg::Hs256,
            b"secret\n",
            serde_json::json!({}),
            DEFAULT_TTL,
        );
        let token = a.sign_at(1).unwrap().token;
        let (input, _, signature) = parts(&token, "HS256");
        let key = hmac::Key::new(hmac::HMAC_SHA256, b"secret");
        hmac::verify(&key, input.as_bytes(), &signature).unwrap();

        let b = signer(
            JwtAlg::Hs256,
            b"secret\n\n",
            serde_json::json!({}),
            DEFAULT_TTL,
        );
        let token = b.sign_at(1).unwrap().token;
        let (input, _, signature) = parts(&token, "HS256");
        let key = hmac::Key::new(hmac::HMAC_SHA256, b"secret\n");
        hmac::verify(&key, input.as_bytes(), &signature).unwrap();

        let error = Signer::new(JwtAlg::Hs256, b"\n", Map::new(), DEFAULT_TTL).unwrap_err();
        assert!(error.contains("empty"), "{error}");
    }

    #[test]
    fn rs256_tokens_verify_with_pkcs8_and_pkcs1_keys() {
        for pem in [RSA_PKCS8, RSA_PKCS1] {
            let signer = signer(
                JwtAlg::Rs256,
                pem.as_bytes(),
                serde_json::json!({}),
                DEFAULT_TTL,
            );
            let SigningKey::Rsa(key) = &signer.key else {
                panic!("expected an RSA key");
            };
            let public = UnparsedPublicKey::new(
                &RSA_PKCS1_2048_8192_SHA256,
                key.public_key().as_ref().to_vec(),
            );
            let token = signer.sign_at(1_700_000_000).unwrap().token;
            let (input, _, signature) = parts(&token, "RS256");
            assert_eq!(signature.len(), 256);
            public.verify(input.as_bytes(), &signature).unwrap();
            let mut tampered = input.into_bytes();
            tampered[0] ^= 1;
            assert!(public.verify(&tampered, &signature).is_err());
        }
    }

    #[test]
    fn es256_tokens_verify_with_a_generated_key() {
        let rng = SystemRandom::new();
        let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng).unwrap();
        let pair = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, pkcs8.as_ref(), &rng)
            .unwrap();
        let public = UnparsedPublicKey::new(
            &ECDSA_P256_SHA256_FIXED,
            pair.public_key().as_ref().to_vec(),
        );
        let key = pem("PRIVATE KEY", pkcs8.as_ref());
        let signer = signer(
            JwtAlg::Es256,
            key.as_bytes(),
            serde_json::json!({}),
            DEFAULT_TTL,
        );
        let token = signer.sign_at(1_700_000_000).unwrap().token;
        let (input, _, signature) = parts(&token, "ES256");
        // JWS uses the fixed-width R || S form, not ASN.1 DER.
        assert_eq!(signature.len(), 64);
        public.verify(input.as_bytes(), &signature).unwrap();
    }

    #[test]
    fn time_claims_follow_the_ttl_and_static_claims_are_merged() {
        let signer = signer(
            JwtAlg::Hs256,
            b"secret",
            serde_json::json!({"sub": "load", "aud": ["ws", "api"], "iss": "wsblast", "n": 3}),
            Duration::from_secs(90),
        );
        let token = signer.sign_at(1_000_000).unwrap();
        assert_eq!(token.expires, 1_000_090);
        let (_, claims, _) = parts(&token.token, "HS256");
        assert_eq!(claims["iat"], 1_000_000);
        assert_eq!(claims["nbf"], 1_000_000);
        assert_eq!(claims["exp"], 1_000_090);
        assert_eq!(claims["sub"], "load");
        assert_eq!(claims["aud"], serde_json::json!(["ws", "api"]));
        assert_eq!(claims["iss"], "wsblast");
        assert_eq!(claims["n"], 3);
        let jti = claims["jti"].as_str().unwrap();
        assert_eq!(b64url_decode(jti).len(), JTI_BYTES);
        assert_eq!(claims.len(), 8);
    }

    #[test]
    fn every_token_gets_a_unique_jti() {
        let signer = signer(JwtAlg::Hs256, b"secret", serde_json::json!({}), DEFAULT_TTL);
        let jtis: HashSet<String> = (0..1000)
            .map(|_| {
                let token = signer.sign_at(1).unwrap().token;
                parts(&token, "HS256").1["jti"]
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect();
        assert_eq!(jtis.len(), 1000);
    }

    fn key_file(name: &str, contents: &[u8]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("wsblast-jwt-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        std::fs::write(&path, contents).unwrap();
        path
    }

    fn config(key_file: PathBuf, ttl: Duration, per_connection: bool) -> JwtConfig {
        JwtConfig {
            alg: JwtAlg::Hs256,
            key_file,
            claims: Map::new(),
            ttl,
            per_connection,
        }
    }

    fn applied(live: &LiveAuthHeader) -> String {
        let mut headers = HeaderMap::new();
        live.apply(&mut headers);
        let value = headers[AUTHORIZATION].to_str().unwrap();
        value.strip_prefix("Bearer ").unwrap().to_string()
    }

    #[test]
    fn per_connection_tokens_are_signed_for_each_attempt() {
        let path = key_file("per-connection", b"secret");
        let (live, _guard) = prepare(&config(path, DEFAULT_TTL, true), AUTHORIZATION).unwrap();
        let jtis: HashSet<String> = (0..200)
            .map(|_| parts(&applied(&live), "HS256").1["jti"].to_string())
            .collect();
        assert_eq!(jtis.len(), 200);
    }

    #[test]
    fn a_shared_token_is_reused_until_it_is_re_signed_before_expiry() {
        let path = key_file("shared", b"secret");
        let ttl = Duration::from_secs(2);
        let (live, _guard) = prepare(&config(path, ttl, false), AUTHORIZATION).unwrap();
        let first = applied(&live);
        assert_eq!(applied(&live), first, "the shared token changed at once");
        let first_exp = parts(&first, "HS256").1["exp"].as_u64().unwrap();

        let deadline = Instant::now() + Duration::from_secs(5);
        let second = loop {
            let token = applied(&live);
            if token != first {
                break token;
            }
            assert!(Instant::now() < deadline, "the token was not re-signed");
            std::thread::sleep(Duration::from_millis(20));
        };
        let claims = parts(&second, "HS256").1;
        let exp = claims["exp"].as_u64().unwrap();
        assert!(exp > first_exp, "{exp} <= {first_exp}");
        // Re-signed before the first token expired.
        assert!(unix_now() < first_exp + 1, "re-signed after expiry");
        assert_eq!(exp - claims["iat"].as_u64().unwrap(), 2);
    }

    #[test]
    fn re_signing_waits_until_shortly_before_expiry() {
        let now = UNIX_EPOCH + Duration::from_secs(1_000);
        let ttl = Duration::from_secs(300);
        assert_eq!(refresh_delay(1_300, ttl, now), Duration::from_secs(240));
        let ttl = Duration::from_secs(3600);
        assert_eq!(refresh_delay(4_600, ttl, now), Duration::from_secs(3540));
        // A token signed late in its first second still leaves a margin.
        let late = UNIX_EPOCH + Duration::from_millis(1_000_900);
        assert_eq!(
            refresh_delay(1_002, Duration::from_secs(2), late),
            Duration::from_millis(700)
        );
        assert_eq!(refresh_delay(900, ttl, now), MIN_REFRESH_DELAY);
    }

    #[test]
    fn key_problems_are_reported_without_the_key() {
        let missing = config(
            PathBuf::from("/nonexistent/wsblast-key"),
            DEFAULT_TTL,
            false,
        );
        let error = Signer::load(&missing).unwrap_err();
        assert!(error.contains("could not read --auth-jwt-key"), "{error}");

        let cases: [(JwtAlg, &[u8], &str); 6] = [
            (JwtAlg::Rs256, b"top-secret-bytes", "not a PEM file"),
            (
                JwtAlg::Es256,
                RSA_PKCS1.as_bytes(),
                "ES256 needs a PEM 'PRIVATE KEY' block, found 'RSA PRIVATE KEY'",
            ),
            (JwtAlg::Es256, RSA_PKCS8.as_bytes(), "not a usable P-256 private key"),
            (
                JwtAlg::Es256,
                b"-----BEGIN EC PRIVATE KEY-----\nAAAA\n-----END EC PRIVATE KEY-----\n",
                "openssl pkcs8 -topk8",
            ),
            (
                JwtAlg::Rs256,
                b"-----BEGIN ENCRYPTED PRIVATE KEY-----\nAAAA\n-----END ENCRYPTED PRIVATE KEY-----\n",
                "encrypted private keys are not supported",
            ),
            (
                JwtAlg::Rs256,
                b"-----BEGIN PRIVATE KEY-----\nAA\n",
                "has no '-----END PRIVATE KEY-----' line",
            ),
        ];
        for (alg, key, message) in cases {
            let error = Signer::new(alg, key, Map::new(), DEFAULT_TTL).unwrap_err();
            assert!(error.contains(message), "{error}");
            assert!(!error.contains("top-secret"), "{error}");
        }
    }

    #[test]
    fn debug_output_hides_the_key() {
        let signer = signer(
            JwtAlg::Hs256,
            b"top-secret-bytes",
            serde_json::json!({}),
            DEFAULT_TTL,
        );
        let debug = format!("{signer:?}");
        assert!(debug.contains("[redacted]"), "{debug}");
        assert!(!debug.contains("top-secret"), "{debug}");
        let path = key_file("debug", b"top-secret-bytes");
        let (live, _guard) = prepare(&config(path, DEFAULT_TTL, false), AUTHORIZATION).unwrap();
        let token = applied(&live);
        assert!(!format!("{live:?}").contains(&token));
    }

    #[test]
    fn options_are_validated() {
        let parse = |extra: &str| {
            let base = ["wsblast", "ws://127.0.0.1:9001", "--auth-type", "jwt"];
            let cli =
                Cli::try_parse_from(base.into_iter().chain(extra.split_whitespace())).unwrap();
            config_from_cli(&cli)
        };
        let config = parse(
            "--auth-jwt-key k.pem --auth-jwt-alg rs256 --auth-jwt-ttl 1h --auth-jwt-per-connection \
             --auth-jwt-claims {\"sub\":\"load\"}",
        )
        .unwrap();
        assert_eq!(config.alg, JwtAlg::Rs256);
        assert_eq!(config.key_file, PathBuf::from("k.pem"));
        assert_eq!(config.ttl, Duration::from_secs(3600));
        assert!(config.per_connection);
        assert_eq!(config.claims["sub"], "load");

        let config = parse("--auth-jwt-key k").unwrap();
        assert_eq!(config.alg, JwtAlg::Hs256);
        assert_eq!(config.ttl, DEFAULT_TTL);
        assert!(!config.per_connection);
        assert!(config.claims.is_empty());

        for (extra, message) in [
            (
                "--auth-jwt-key k --auth-jwt-claims [1]",
                "must be a JSON object",
            ),
            ("--auth-jwt-key k --auth-jwt-claims {", "not valid JSON"),
            (
                "--auth-jwt-key k --auth-jwt-claims {\"exp\":1}",
                "must not set 'exp'",
            ),
            (
                "--auth-jwt-key k --auth-jwt-claims {\"jti\":\"x\"}",
                "must not set 'jti'",
            ),
            ("--auth-jwt-key k --auth-jwt-ttl 1s", "at least 2s"),
            ("--auth-jwt-key k --auth-jwt-ttl soon", "Invalid duration"),
        ] {
            let error = parse(extra).unwrap_err().to_string();
            assert!(error.contains(message), "{extra}: {error}");
        }
    }
}
