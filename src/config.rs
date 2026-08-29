use std::{fmt, time::Duration};

use thiserror::Error;
use url::Url;

const DEFAULT_API_URL: &str = "https://api.hackmd.io/v1";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_RETRIES: u8 = 3;
const INITIAL_RETRY_BACKOFF: Duration = Duration::from_millis(500);
const MAX_RETRY_BACKOFF: Duration = Duration::from_secs(5);

/// Application configuration loaded from the local process environment.
#[derive(Debug)]
pub(crate) struct Config {
    api_token: Option<SecretToken>,
    #[allow(dead_code, reason = "consumed by the following HTTP client task")]
    api_url: Url,
    #[allow(dead_code, reason = "consumed by the following HTTP client task")]
    request_timeout: Duration,
    #[allow(dead_code, reason = "consumed by the following HTTP client task")]
    connect_timeout: Duration,
    #[allow(dead_code, reason = "consumed by the following HTTP client task")]
    retry: RetryConfig,
}

impl Config {
    pub(crate) fn from_env() -> Result<Self, ConfigError> {
        Self::from_getter(|key| std::env::var(key).ok())
    }

    fn from_getter(mut get: impl FnMut(&str) -> Option<String>) -> Result<Self, ConfigError> {
        let api_token = get("HACKMD_API_TOKEN")
            .filter(|token| !token.trim().is_empty())
            .map(SecretToken);
        let api_url = get("HACKMD_API_URL").unwrap_or_else(|| DEFAULT_API_URL.to_owned());

        Ok(Self {
            api_token,
            api_url: Url::parse(&api_url).map_err(ConfigError::InvalidApiUrl)?,
            request_timeout: REQUEST_TIMEOUT,
            connect_timeout: CONNECT_TIMEOUT,
            retry: RetryConfig::default(),
        })
    }

    pub(crate) fn has_api_token(&self) -> bool {
        self.api_token.is_some()
    }

    #[cfg(test)]
    pub(crate) fn for_tests() -> Self {
        Self::from_getter(|_| None).expect("hard-coded defaults must remain valid")
    }
}

/// Retry limits used by operations that are safe to retry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RetryConfig {
    pub(crate) max_retries: u8,
    pub(crate) initial_backoff: Duration,
    pub(crate) max_backoff: Duration,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_retries: MAX_RETRIES,
            initial_backoff: INITIAL_RETRY_BACKOFF,
            max_backoff: MAX_RETRY_BACKOFF,
        }
    }
}

struct SecretToken(#[allow(dead_code, reason = "used by the following HTTP client task")] String);

impl fmt::Debug for SecretToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("[REDACTED]")
    }
}

#[derive(Debug, Error)]
pub(crate) enum ConfigError {
    #[error("HACKMD_API_URL must be a valid URL")]
    InvalidApiUrl(#[source] url::ParseError),
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, time::Duration};

    use super::{Config, DEFAULT_API_URL};

    fn config_from(entries: &[(&str, &str)]) -> Result<Config, super::ConfigError> {
        let values: HashMap<&str, &str> = entries.iter().copied().collect();
        Config::from_getter(|key| values.get(key).map(ToString::to_string))
    }

    #[test]
    fn defaults_are_bounded_and_token_is_optional() {
        let config = config_from(&[]).expect("defaults should be valid");

        assert!(!config.has_api_token());
        assert_eq!(config.api_url.as_str(), DEFAULT_API_URL);
        assert_eq!(config.request_timeout, Duration::from_secs(30));
        assert_eq!(config.connect_timeout, Duration::from_secs(10));
        assert_eq!(config.retry.max_retries, 3);
        assert_eq!(config.retry.initial_backoff, Duration::from_millis(500));
        assert_eq!(config.retry.max_backoff, Duration::from_secs(5));
    }

    #[test]
    fn environment_overrides_url_and_token() {
        let config = config_from(&[
            ("HACKMD_API_TOKEN", "test-secret"),
            ("HACKMD_API_URL", "https://example.test/custom"),
        ])
        .expect("overrides should be valid");

        assert!(config.has_api_token());
        assert_eq!(config.api_url.as_str(), "https://example.test/custom");
    }

    #[test]
    fn empty_token_is_treated_as_missing() {
        let config = config_from(&[("HACKMD_API_TOKEN", "  ")])
            .expect("an empty token should not prevent startup");

        assert!(!config.has_api_token());
    }

    #[test]
    fn debug_output_redacts_token() {
        let config = config_from(&[("HACKMD_API_TOKEN", "test-secret")])
            .expect("token should be accepted without validation");
        let debug = format!("{config:?}");

        assert!(!debug.contains("test-secret"));
        assert!(debug.contains("[REDACTED]"));
    }

    #[test]
    fn invalid_api_url_is_actionable_without_echoing_input() {
        let invalid = "not a url with secret material";
        let error = config_from(&[("HACKMD_API_URL", invalid)])
            .expect_err("invalid URL should be rejected");

        assert_eq!(error.to_string(), "HACKMD_API_URL must be a valid URL");
        assert!(!error.to_string().contains(invalid));
    }
}
