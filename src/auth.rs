//! Static token and API key authentication for WebSocket handshakes.

#[cfg(feature = "auth")]
use crate::config::AuthConfig;
#[cfg(feature = "auth")]
use crate::error::WorkerError;

/// Resolves the configured authentication method into one handshake header.
///
/// The header is resolved once before the load test starts and sent on every
/// handshake, including reconnects.
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
    fn invalid_header_name_is_rejected() {
        let error = resolve_auth_header(&AuthConfig::Static {
            token: "token".to_string(),
            header: "bad header".to_string(),
        })
        .unwrap_err();

        assert!(error.to_string().contains("invalid authentication header"));
    }
}
