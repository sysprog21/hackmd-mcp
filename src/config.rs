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
/// How long a fetched note list stays usable. Short enough that a stale list is
/// a momentary annoyance, long enough to absorb an agent resolving several note
/// URLs in a row.
const LIST_CACHE_TTL: Duration = Duration::from_secs(60);
const SUPPORTED_ENV_KEYS: [&str; 4] = [
    "HACKMD_API_TOKEN",
    "HACKMD_API_URL",
    "HACKMD_MCP_STATE_DIR",
    "HACKMD_MCP_WORKSPACE_ROOT",
];

/// Application configuration loaded from the local process environment.
#[derive(Debug)]
pub(crate) struct Config {
    api_token: Option<SecretToken>,
    api_url: Url,
    request_timeout: Duration,
    connect_timeout: Duration,
    retry: RetryConfig,
    state_dir: PathBuf,
    workspace_root: Option<PathBuf>,
    list_cache_ttl: Duration,
}

impl Config {
    pub(crate) fn from_env() -> Result<Self, ConfigError> {
        let dotenv = std::env::current_dir()
            .ok()
            .map(|directory| load_dotenv(&directory.join(".env")))
            .unwrap_or_default();
        let inherited = |key: &str| std::env::var(key).ok().filter(|v| !v.trim().is_empty());

        // The working directory is often someone else's repository. A `.env`
        // there must not be able to point an inherited token at a host of its
        // author's choosing, which would hand them the credential on the first
        // request. An inherited token wins over one in the file, so a `.env`
        // token does not make this safe; local development is unaffected as
        // long as the environment carries no token of its own.
        if inherited("HACKMD_API_TOKEN").is_some()
            && inherited("HACKMD_API_URL").is_none()
            && dotenv.contains_key("HACKMD_API_URL")
        {
            return Err(ConfigError::DotenvApiUrlWithInheritedToken);
        }
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
        let workspace_root = get("HACKMD_MCP_WORKSPACE_ROOT")
            .filter(|path| !path.trim().is_empty())
            .map(PathBuf::from);
        if workspace_root
            .as_ref()
            .is_some_and(|root| !root.is_absolute())
        {
            return Err(ConfigError::RelativeWorkspaceRoot);
        }

        let api_url = Url::parse(&api_url).map_err(ConfigError::InvalidApiUrl)?;
        validate_api_url(&api_url, policy)?;

        Ok(Self {
            api_token,
            api_url,
            request_timeout: REQUEST_TIMEOUT,
            connect_timeout: CONNECT_TIMEOUT,
            retry: RetryConfig::default(),
            state_dir,
            workspace_root,
            list_cache_ttl: LIST_CACHE_TTL,
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

    /// Optional tree the local sync tools are confined to. `None` keeps the
    /// original behavior of accepting any absolute path.
    pub(crate) fn workspace_root(&self) -> Option<&Path> {
        self.workspace_root.as_deref()
    }

    pub(crate) const fn retry(&self) -> RetryConfig {
        self.retry
    }

    pub(crate) const fn list_cache_ttl(&self) -> Duration {
        self.list_cache_ttl
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
        let mut config = Self::with_loopback_http_for_tests(|key| match key {
            "HACKMD_API_URL" => Some(api_url.to_owned()),
            "HACKMD_API_TOKEN" => token.map(str::to_owned),
            _ => None,
        })
        .expect("loopback test URL must be valid");

        // Off by default in tests: request-count assertions are the point of
        // the fixtures, and a cache hit would silently swallow one.
        config.list_cache_ttl = Duration::ZERO;
        config
    }

    #[cfg(test)]
    pub(crate) fn for_loopback_test_with_cache(api_url: &str) -> Self {
        Self {
            list_cache_ttl: LIST_CACHE_TTL,
            ..Self::for_loopback_test(api_url, Some(crate::fixture::FIXTURE_TOKEN))
        }
    }

    #[cfg(test)]
    pub(crate) fn for_loopback_test_with_timeout(
        api_url: &str,
        token: &str,
        request_timeout: Duration,
    ) -> Self {
        let mut config = Self::for_loopback_test(api_url, Some(token));
        config.request_timeout = request_timeout;
        config.retry.max_retries = 0;
        config
    }

    #[cfg(test)]
    pub(crate) fn for_loopback_test_with_retry(
        api_url: &str,
        token: &str,
        retry: RetryConfig,
    ) -> Self {
        let mut config = Self::for_loopback_test(api_url, Some(token));
        config.retry = retry;
        config
    }

    #[cfg(test)]
    pub(crate) fn for_loopback_test_no_retry(api_url: &str, token: &str) -> Self {
        Self::for_loopback_test_with_retry(
            api_url,
            token,
            RetryConfig {
                max_retries: 0,
                initial_backoff: Duration::ZERO,
                max_backoff: Duration::ZERO,
            },
        )
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
    let directories = BaseDirs::new().ok_or(ConfigError::StateDirectoryUnavailable)?;

    // `state_dir` is Some only on Linux, where XDG defines one. macOS and
    // Windows have no separate state location, so their private application
    // data directory serves instead.
    let root = directories
        .state_dir()
        .unwrap_or_else(|| directories.data_local_dir());
    Ok(root.join("hackmd-mcp"))
}

fn load_dotenv(path: &Path) -> HashMap<String, String> {
    let Ok(contents) = std::fs::read_to_string(path) else {
        return HashMap::new();
    };
    warn_if_readable_by_others(path);

    contents.lines().filter_map(parse_dotenv_line).collect()
}

/// The `.env` this just read holds an API token. Its permissions are the user's
/// to set, but a file every local account can read is worth one line on stderr,
/// which is the only moment anyone will look.
fn warn_if_readable_by_others(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let Ok(metadata) = std::fs::metadata(path) else {
            return;
        };
        let mode = metadata.permissions().mode();
        if mode & 0o077 != 0 {
            tracing::warn!(
                path = %path.display(),
                mode = format!("{:o}", mode & 0o777),
                "dotenv file is readable by other accounts; chmod 600 it"
            );
        }
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
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
    #[error("HACKMD_MCP_WORKSPACE_ROOT must be an absolute path")]
    RelativeWorkspaceRoot,
    #[error(
        "the working-directory .env sets HACKMD_API_URL while HACKMD_API_TOKEN comes from the environment, which would send that token to the endpoint the file names; set HACKMD_API_URL in the environment too, or unset HACKMD_API_TOKEN so the .env values apply as one pair"
    )]
    DotenvApiUrlWithInheritedToken,
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
    fn workspace_root_must_be_absolute() {
        let error = config_from(&[("HACKMD_MCP_WORKSPACE_ROOT", "relative/notes")])
            .expect_err("relative workspace root should be rejected");
        assert_eq!(
            error.to_string(),
            "HACKMD_MCP_WORKSPACE_ROOT must be an absolute path"
        );
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
