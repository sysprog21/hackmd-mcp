use std::{
    collections::HashMap,
    fmt,
    path::{Path, PathBuf},
    time::Duration,
};

use directories::BaseDirs;
use thiserror::Error;
#[cfg(test)]
use url::Host;
use url::Url;

const DEFAULT_API_URL: &str = "https://api.hackmd.io/v1";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_RETRIES: u8 = 3;
const INITIAL_RETRY_BACKOFF: Duration = Duration::from_millis(500);
const MAX_RETRY_BACKOFF: Duration = Duration::from_secs(5);
const SUPPORTED_ENV_KEYS: [&str; 3] =
    ["HACKMD_API_TOKEN", "HACKMD_API_URL", "HACKMD_MCP_STATE_DIR"];

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
    state_dir: PathBuf,
}

impl Config {
    pub(crate) fn from_env() -> Result<Self, ConfigError> {
        let dotenv = std::env::current_dir()
            .ok()
            .map(|directory| load_dotenv(&directory.join(".env")))
            .unwrap_or_default();
        Self::from_getter(|key| std::env::var(key).ok().or_else(|| dotenv.get(key).cloned()))
    }

    fn from_getter(mut get: impl FnMut(&str) -> Option<String>) -> Result<Self, ConfigError> {
        Self::from_getter_with_policy(&mut get, ApiUrlPolicy::HttpsOnly)
    }

    fn from_getter_with_policy(
        mut get: impl FnMut(&str) -> Option<String>,
        policy: ApiUrlPolicy,
    ) -> Result<Self, ConfigError> {
        let api_token = get("HACKMD_API_TOKEN")
            .filter(|token| !token.trim().is_empty())
            .map(SecretToken);
        let api_url = get("HACKMD_API_URL").unwrap_or_else(|| DEFAULT_API_URL.to_owned());
        let state_dir = get("HACKMD_MCP_STATE_DIR")
            .filter(|path| !path.trim().is_empty())
            .map_or_else(default_state_dir, |path| Ok(PathBuf::from(path)))?;

        let api_url = Url::parse(&api_url).map_err(ConfigError::InvalidApiUrl)?;
        validate_api_url(&api_url, policy)?;

        Ok(Self {
            api_token,
            api_url,
            request_timeout: REQUEST_TIMEOUT,
            connect_timeout: CONNECT_TIMEOUT,
            retry: RetryConfig::default(),
            state_dir,
        })
    }

    pub(crate) fn has_api_token(&self) -> bool {
        self.api_token.is_some()
    }

    pub(crate) fn api_token(&self) -> Option<&str> {
        self.api_token.as_ref().map(SecretToken::expose)
    }

    pub(crate) fn api_url(&self) -> &Url {
        &self.api_url
    }

    pub(crate) fn request_timeout(&self) -> Duration {
        self.request_timeout
    }

    pub(crate) fn connect_timeout(&self) -> Duration {
        self.connect_timeout
    }

    pub(crate) fn state_dir(&self) -> &Path {
        &self.state_dir
    }

    #[cfg(test)]
    pub(crate) fn for_tests() -> Self {
        Self::from_getter(|_| None).expect("hard-coded defaults must remain valid")
    }

    #[cfg(test)]
    fn with_loopback_http_for_tests(
        get: impl FnMut(&str) -> Option<String>,
    ) -> Result<Self, ConfigError> {
        Self::from_getter_with_policy(get, ApiUrlPolicy::AllowLoopbackHttp)
    }

    #[cfg(test)]
    pub(crate) fn for_loopback_test(api_url: &str, token: Option<&str>) -> Self {
        Self::with_loopback_http_for_tests(|key| match key {
            "HACKMD_API_URL" => Some(api_url.to_owned()),
            "HACKMD_API_TOKEN" => token.map(str::to_owned),
            _ => None,
        })
        .expect("loopback test URL must be valid")
    }

    #[cfg(test)]
    pub(crate) fn for_loopback_test_with_timeout(
        api_url: &str,
        token: &str,
        request_timeout: Duration,
    ) -> Self {
        let mut config = Self::for_loopback_test(api_url, Some(token));
        config.request_timeout = request_timeout;
        config
    }
}

#[derive(Clone, Copy)]
enum ApiUrlPolicy {
    HttpsOnly,
    #[cfg(test)]
    AllowLoopbackHttp,
}

fn validate_api_url(url: &Url, policy: ApiUrlPolicy) -> Result<(), ConfigError> {
    if url.scheme() == "https" {
        return Ok(());
    }
    #[cfg(test)]
    if matches!(policy, ApiUrlPolicy::AllowLoopbackHttp)
        && url.scheme() == "http"
        && url.host().is_some_and(|host| is_loopback_host(&host))
    {
        return Ok(());
    }
    let _ = policy;
    Err(ConfigError::InsecureApiUrl)
}

#[cfg(test)]
fn is_loopback_host(host: &Host<&str>) -> bool {
    match host {
        Host::Domain(domain) => domain.eq_ignore_ascii_case("localhost"),
        Host::Ipv4(address) => address.is_loopback(),
        Host::Ipv6(address) => address.is_loopback(),
    }
}

fn default_state_dir() -> Result<PathBuf, ConfigError> {
    BaseDirs::new()
        .and_then(|directories| directories.state_dir().map(Path::to_path_buf))
        .map(|state_dir| state_dir.join("hackmd-mcp"))
        .ok_or(ConfigError::StateDirectoryUnavailable)
}

fn load_dotenv(path: &Path) -> HashMap<String, String> {
    let Ok(contents) = std::fs::read_to_string(path) else {
        return HashMap::new();
    };

    contents.lines().filter_map(parse_dotenv_line).collect()
}

fn parse_dotenv_line(line: &str) -> Option<(String, String)> {
    let line = line.trim().strip_prefix("export ").unwrap_or(line.trim());
    if line.is_empty() || line.starts_with('#') {
        return None;
    }

    let (key, raw_value) = line.split_once('=')?;
    let key = key.trim();
    if !SUPPORTED_ENV_KEYS.contains(&key) {
        return None;
    }

    parse_dotenv_value(raw_value).map(|value| (key.to_owned(), value))
}

fn parse_dotenv_value(raw: &str) -> Option<String> {
    let value = raw.trim();
    if let Some(quoted) = value.strip_prefix('"') {
        return quoted.strip_suffix('"').map(str::to_owned);
    }
    if let Some(quoted) = value.strip_prefix('\'') {
        return quoted.strip_suffix('\'').map(str::to_owned);
    }

    let value = value
        .split_once(" #")
        .map_or(value, |(before_comment, _)| before_comment);
    Some(value.trim_end().to_owned())
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

struct SecretToken(String);

impl SecretToken {
    fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SecretToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("[REDACTED]")
    }
}

#[derive(Debug, Error)]
pub(crate) enum ConfigError {
    #[error("HACKMD_API_URL must be a valid URL")]
    InvalidApiUrl(#[source] url::ParseError),
    #[error("HACKMD_API_URL must use HTTPS")]
    InsecureApiUrl,
    #[error("platform state directory is unavailable; set HACKMD_MCP_STATE_DIR")]
    StateDirectoryUnavailable,
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, fs, time::Duration};

    use super::{Config, DEFAULT_API_URL, load_dotenv, parse_dotenv_line};

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
        assert!(config.state_dir.ends_with("hackmd-mcp"));
    }

    #[test]
    fn environment_overrides_url_and_token() {
        let config = config_from(&[
            ("HACKMD_API_TOKEN", "test-secret"),
            ("HACKMD_API_URL", "https://example.test/custom"),
            ("HACKMD_MCP_STATE_DIR", "/tmp/hackmd-mcp-test-state"),
        ])
        .expect("overrides should be valid");

        assert!(config.has_api_token());
        assert_eq!(config.api_url.as_str(), "https://example.test/custom");
        assert_eq!(
            config.state_dir,
            std::path::Path::new("/tmp/hackmd-mcp-test-state")
        );
    }

    #[test]
    fn empty_token_is_treated_as_missing() {
        let config = config_from(&[("HACKMD_API_TOKEN", "  ")])
            .expect("an empty token should not prevent startup");

        assert!(!config.has_api_token());
    }

    #[test]
    fn empty_state_directory_override_uses_the_platform_default() {
        let config = config_from(&[("HACKMD_MCP_STATE_DIR", "  ")])
            .expect("an empty state override should use the safe default");

        assert!(config.state_dir.ends_with("hackmd-mcp"));
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

    #[test]
    fn normal_configuration_rejects_non_https_urls() {
        for insecure in [
            "http://api.hackmd.io/v1",
            "http://127.0.0.1:8080/v1",
            "file:///tmp/fake-api",
        ] {
            let error = config_from(&[("HACKMD_API_URL", insecure)])
                .expect_err("normal configuration must require HTTPS");
            assert_eq!(error.to_string(), "HACKMD_API_URL must use HTTPS");
            assert!(!error.to_string().contains(insecure));
        }
    }

    #[test]
    fn test_policy_allows_only_loopback_http() {
        for loopback in [
            "http://localhost:8080/v1",
            "http://127.0.0.1:8080/v1",
            "http://[::1]:8080/v1",
        ] {
            let values = HashMap::from([("HACKMD_API_URL", loopback)]);
            let config = Config::with_loopback_http_for_tests(|key| {
                values.get(key).map(ToString::to_string)
            })
            .expect("test policy should allow loopback HTTP");
            assert_eq!(config.api_url.as_str(), loopback);
        }

        let values = HashMap::from([("HACKMD_API_URL", "http://example.test/v1")]);
        let error =
            Config::with_loopback_http_for_tests(|key| values.get(key).map(ToString::to_string))
                .expect_err("test policy must reject remote HTTP");
        assert_eq!(error.to_string(), "HACKMD_API_URL must use HTTPS");
    }

    #[test]
    fn dotenv_parser_accepts_common_forms_and_only_supported_keys() {
        assert_eq!(
            parse_dotenv_line("HACKMD_API_URL=https://example.test/v1 # local endpoint"),
            Some((
                "HACKMD_API_URL".to_owned(),
                "https://example.test/v1".to_owned()
            ))
        );
        assert_eq!(
            parse_dotenv_line("export HACKMD_API_TOKEN='quoted token'"),
            Some(("HACKMD_API_TOKEN".to_owned(), "quoted token".to_owned()))
        );
        assert_eq!(parse_dotenv_line("UNRELATED_SECRET=ignore-me"), None);
        assert_eq!(parse_dotenv_line("# HACKMD_API_TOKEN=commented"), None);
        assert_eq!(parse_dotenv_line("malformed"), None);
    }

    #[test]
    fn dotenv_file_is_a_fallback_to_inherited_values() {
        let directory = tempfile::tempdir().expect("temporary directory should be created");
        let path = directory.path().join(".env");
        fs::write(
            &path,
            "HACKMD_API_TOKEN=dotenv-token\nHACKMD_API_URL=https://dotenv.example/v1\n",
        )
        .expect("dotenv fixture should be written");
        let dotenv = load_dotenv(&path);
        let inherited = HashMap::from([(
            "HACKMD_API_URL".to_owned(),
            "https://inherited.example/v1".to_owned(),
        )]);
        let config = Config::from_getter(|key| {
            inherited
                .get(key)
                .cloned()
                .or_else(|| dotenv.get(key).cloned())
        })
        .expect("merged configuration should be valid");

        assert!(config.has_api_token());
        assert_eq!(config.api_url.as_str(), "https://inherited.example/v1");
    }

    #[test]
    fn missing_or_non_utf8_dotenv_is_quietly_ignored() {
        let directory = tempfile::tempdir().expect("temporary directory should be created");
        assert!(load_dotenv(&directory.path().join("missing.env")).is_empty());

        let path = directory.path().join("non-utf8.env");
        fs::write(&path, [0xff, 0xfe]).expect("dotenv fixture should be written");
        assert!(load_dotenv(&path).is_empty());
    }
}
