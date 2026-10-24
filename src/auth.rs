//! Authentication module for WebSocket load testing.
//! Provides various authentication mechanisms for WebSocket handshakes.

#[cfg(feature = "auth")]
use crate::config::AuthConfig;
#[cfg(feature = "auth")]
use crate::error::WorkerError;
#[cfg(feature = "auth")]
use chrono;
#[cfg(feature = "auth")]
use jsonwebtoken::{EncodingKey, Header, encode};
#[cfg(feature = "auth")]
use oauth2::{EmptyExtraTokenFields, StandardTokenResponse, TokenResponse, basic::BasicTokenType};
#[cfg(feature = "auth")]
use reqwest::blocking::Client;
#[cfg(feature = "auth")]
use std::time::{Duration, Instant};

#[cfg(feature = "auth")]
/// Trait for authentication providers that can generate tokens for WebSocket handshakes.
pub trait Authenticator: Send + Sync {
    /// Returns the current authentication token.
    fn get_token(&mut self) -> std::result::Result<String, WorkerError>;

    /// Refreshes the token if needed (e.g., before expiry).
    fn refresh_if_needed(&mut self) -> std::result::Result<(), WorkerError>;
}

#[cfg(feature = "auth")]
/// Static token authenticator - uses a pre-configured bearer token.
pub struct StaticAuthenticator {
    token: String,
    header: String,
}

#[cfg(feature = "auth")]
impl StaticAuthenticator {
    pub fn new(token: String, header: String) -> Self {
        Self { token, header }
    }
}

#[cfg(feature = "auth")]
impl Authenticator for StaticAuthenticator {
    fn get_token(&mut self) -> std::result::Result<String, WorkerError> {
        Ok(format!("{} {}", self.header, self.token))
    }

    fn refresh_if_needed(&mut self) -> std::result::Result<(), WorkerError> {
        Ok(())
    }
}

#[cfg(feature = "auth")]
/// JWT authenticator that locally signs an HS256 client-assertion token.
///
/// RS256/JWKS client-credentials flows are not implemented; the CLI rejects
/// them at startup instead of accepting a configuration that cannot work.
pub struct JwtAuthenticator {
    endpoint: url::Url,
    client_id: String,
    secret: String,
    scopes: Vec<String>,
    cached_token: Option<String>,
    token_expiry: Option<Instant>,
}

#[cfg(feature = "auth")]
impl JwtAuthenticator {
    pub fn new(endpoint: url::Url, client_id: String, secret: String, scopes: Vec<String>) -> Self {
        Self {
            endpoint,
            client_id,
            secret,
            scopes,
            cached_token: None,
            token_expiry: None,
        }
    }
}

#[cfg(feature = "auth")]
impl Authenticator for JwtAuthenticator {
    fn get_token(&mut self) -> std::result::Result<String, WorkerError> {
        // Check cached token
        if let (Some(token), Some(expiry)) = (&self.cached_token, self.token_expiry) {
            if Instant::now() < expiry - Duration::from_secs(30) {
                return Ok(format!("Bearer {}", token));
            }
        }

        let now = chrono::Utc::now().timestamp() as usize;
        let claims = serde_json::json!({
            "iss": self.client_id,
            "sub": self.client_id,
            "aud": self.endpoint.to_string(),
            "exp": now + 3600,
            "iat": now,
            "scope": self.scopes.join(" "),
        });

        let header = Header {
            alg: jsonwebtoken::Algorithm::HS256,
            ..Default::default()
        };
        let token = encode(
            &header,
            &claims,
            &EncodingKey::from_secret(self.secret.as_bytes()),
        )
        .map_err(|e| WorkerError::Protocol(format!("JWT encoding failed: {e}")))?;

        self.cached_token = Some(token.clone());
        self.token_expiry = Some(Instant::now() + Duration::from_secs(3600));

        Ok(format!("Bearer {}", token))
    }

    fn refresh_if_needed(&mut self) -> std::result::Result<(), WorkerError> {
        self.get_token()?;
        Ok(())
    }
}

#[cfg(feature = "auth")]
/// OAuth2 client credentials authenticator - uses blocking reqwest for token exchange.
pub struct OAuth2Authenticator {
    token_url: String,
    client_id: String,
    client_secret: String,
    scopes: Vec<String>,
    http_client: Client,
    cached_token: Option<String>,
    token_expiry: Option<Instant>,
}

#[cfg(feature = "auth")]
impl OAuth2Authenticator {
    pub fn new(
        token_url: url::Url,
        client_id: String,
        client_secret: String,
        scopes: Vec<String>,
    ) -> std::result::Result<Self, WorkerError> {
        let http_client = Client::new();
        Ok(Self {
            token_url: token_url.to_string(),
            client_id,
            client_secret,
            scopes,
            http_client,
            cached_token: None,
            token_expiry: None,
        })
    }

    fn exchange_token(
        &mut self,
        params: &[(&str, &str)],
    ) -> std::result::Result<
        StandardTokenResponse<EmptyExtraTokenFields, BasicTokenType>,
        WorkerError,
    > {
        let response = self
            .http_client
            .post(&self.token_url)
            .form(&params)
            .send()
            .map_err(|e| WorkerError::Protocol(format!("OAuth2 token request failed: {e}")))?;

        let status = response.status();
        if !status.is_success() {
            let error_text = response.text().unwrap_or_default();
            return Err(WorkerError::Protocol(format!(
                "OAuth2 token request failed with status {}: {}",
                status, error_text
            )));
        }

        response
            .json::<StandardTokenResponse<EmptyExtraTokenFields, BasicTokenType>>()
            .map_err(|e| WorkerError::Protocol(format!("OAuth2 token response parse failed: {e}")))
    }
}

#[cfg(feature = "auth")]
impl Authenticator for OAuth2Authenticator {
    fn get_token(&mut self) -> std::result::Result<String, WorkerError> {
        // Check cached token
        if let (Some(token), Some(expiry)) = (&self.cached_token, self.token_expiry) {
            if Instant::now() < expiry - Duration::from_secs(30) {
                return Ok(format!("Bearer {}", token));
            }
        }

        // Exchange client credentials for token
        let client_id = self.client_id.clone();
        let client_secret = self.client_secret.clone();
        let scope = self.scopes.join(" ");
        let mut params = vec![
            ("grant_type", "client_credentials"),
            ("client_id", client_id.as_str()),
            ("client_secret", client_secret.as_str()),
        ];
        if !scope.is_empty() {
            params.push(("scope", scope.as_str()));
        }

        let token_response = self.exchange_token(&params)?;

        let token = token_response.access_token().secret().to_string();
        let expires_in = token_response
            .expires_in()
            .unwrap_or(Duration::from_secs(3600));
        self.cached_token = Some(token.clone());
        self.token_expiry = Some(Instant::now() + expires_in);

        Ok(format!("Bearer {}", token))
    }

    fn refresh_if_needed(&mut self) -> std::result::Result<(), WorkerError> {
        self.get_token()?;
        Ok(())
    }
}

#[cfg(feature = "auth")]
/// API Key authenticator - uses a simple API key header.
pub struct ApiKeyAuthenticator {
    key: String,
    header: String,
}

#[cfg(feature = "auth")]
impl ApiKeyAuthenticator {
    pub fn new(key: String, header: String) -> Self {
        Self { key, header }
    }
}

#[cfg(feature = "auth")]
impl Authenticator for ApiKeyAuthenticator {
    fn get_token(&mut self) -> std::result::Result<String, WorkerError> {
        Ok(format!("{}: {}", self.header, self.key))
    }

    fn refresh_if_needed(&mut self) -> std::result::Result<(), WorkerError> {
        Ok(())
    }
}

/// Resolves the configured authentication method into one handshake header.
///
/// Tokens are acquired once before the load test starts. Sessions that outlive
/// the token lifetime require a refreshed configuration; automatic mid-run
/// rotation is not performed here.
#[cfg(feature = "auth")]
pub fn resolve_auth_header(
    auth: &AuthConfig,
) -> std::result::Result<(http::header::HeaderName, http::header::HeaderValue), WorkerError> {
    let (header, value) = match auth {
        AuthConfig::Static { token, header } => {
            let value = if header.eq_ignore_ascii_case("authorization")
                && !token.to_ascii_lowercase().starts_with("bearer ")
            {
                format!("Bearer {token}")
            } else {
                token.clone()
            };
            (header.clone(), value)
        }
        AuthConfig::ApiKey { key, header } => (header.clone(), key.clone()),
        AuthConfig::Jwt {
            endpoint,
            client_id,
            secret,
            scopes,
        } => {
            let mut authenticator = JwtAuthenticator::new(
                endpoint.clone(),
                client_id.clone(),
                secret.clone(),
                scopes.clone(),
            );
            ("Authorization".to_string(), authenticator.get_token()?)
        }
        AuthConfig::OAuth2 {
            token_url,
            client_id,
            client_secret,
            scopes,
        } => {
            let mut authenticator = OAuth2Authenticator::new(
                token_url.clone(),
                client_id.clone(),
                client_secret.clone(),
                scopes.clone(),
            )?;
            ("Authorization".to_string(), authenticator.get_token()?)
        }
        AuthConfig::Custom { .. } => {
            return Err(WorkerError::Protocol(
                "custom authentication is not supported".to_string(),
            ));
        }
    };

    let name = http::header::HeaderName::from_bytes(header.as_bytes()).map_err(|e| {
        WorkerError::Protocol(format!("invalid authentication header '{header}': {e}"))
    })?;
    let value = http::header::HeaderValue::from_str(&value)
        .map_err(|e| WorkerError::Protocol(format!("invalid authentication header value: {e}")))?;

    Ok((name, value))
}

/// Resolves an authentication configuration and applies it to handshake headers.
///
/// Existing headers with the same name are replaced.
#[cfg(feature = "auth")]
pub fn apply_auth_header(
    headers: &mut http::HeaderMap,
    auth: &AuthConfig,
) -> std::result::Result<(), WorkerError> {
    let (name, value) = resolve_auth_header(auth)?;
    headers.insert(name, value);
    Ok(())
}

#[cfg(all(test, feature = "auth"))]
mod tests {
    use super::*;

    #[test]
    fn static_authorization_gets_bearer_prefix() {
        let (name, value) = resolve_auth_header(&AuthConfig::Static {
            token: "token-123".to_string(),
            header: "Authorization".to_string(),
        })
        .unwrap();

        assert_eq!(name.as_str(), "authorization");
        assert_eq!(value.to_str().unwrap(), "Bearer token-123");
    }

    #[test]
    fn static_bearer_token_is_not_prefixed_twice() {
        let (_, value) = resolve_auth_header(&AuthConfig::Static {
            token: "Bearer token-123".to_string(),
            header: "Authorization".to_string(),
        })
        .unwrap();

        assert_eq!(value.to_str().unwrap(), "Bearer token-123");
    }

    #[test]
    fn static_custom_header_preserves_raw_token() {
        let (name, value) = resolve_auth_header(&AuthConfig::Static {
            token: "token-123".to_string(),
            header: "X-Session".to_string(),
        })
        .unwrap();

        assert_eq!(name.as_str(), "x-session");
        assert_eq!(value.to_str().unwrap(), "token-123");
    }

    #[test]
    fn api_key_uses_configured_header() {
        let (name, value) = resolve_auth_header(&AuthConfig::ApiKey {
            key: "secret".to_string(),
            header: "X-Api-Key".to_string(),
        })
        .unwrap();

        assert_eq!(name.as_str(), "x-api-key");
        assert_eq!(value.to_str().unwrap(), "secret");
    }

    #[test]
    fn custom_auth_is_rejected() {
        let error = resolve_auth_header(&AuthConfig::Custom {
            refresh_fn: "libauth.so".to_string(),
        })
        .unwrap_err();

        assert!(error.to_string().contains("custom authentication"));
    }

    #[test]
    fn invalid_header_name_is_rejected() {
        let error = resolve_auth_header(&AuthConfig::Static {
            token: "token".to_string(),
            header: "bad header".to_string(),
        })
        .unwrap_err();

        assert!(error.to_string().contains("invalid authentication header"));
    }
}
