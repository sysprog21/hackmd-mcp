//! How a failed request is reported: the error type every tool surfaces, and
//! the mapping from status codes and rate-limit headers onto it.

use reqwest::StatusCode;
use thiserror::Error;

pub(super) fn request_error(error: &reqwest::Error, method: String, path: String) -> HackmdError {
    if error.is_timeout() {
        HackmdError::Timeout { method, path }
    } else {
        HackmdError::Network { method, path }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct RateLimitHeaders {
    user_limit: Option<u32>,
    user_remaining: Option<u32>,
    reset_after: Option<u64>,
}

impl RateLimitHeaders {
    pub(super) fn from_headers(headers: &reqwest::header::HeaderMap) -> Self {
        Self {
            user_limit: parse_header(headers, "x-ratelimit-userlimit"),
            user_remaining: parse_header(headers, "x-ratelimit-userremaining"),
            reset_after: parse_header(headers, "x-ratelimit-userreset"),
        }
    }

    fn detail(self) -> String {
        match (self.user_remaining, self.user_limit, self.reset_after) {
            (None, None, None) => "quota headers unavailable".to_owned(),
            (remaining, limit, reset) => format!(
                "remaining {}/{}, reset after {} seconds",
                optional_number(remaining),
                optional_number(limit),
                optional_number(reset)
            ),
        }
    }
}

fn parse_header<T>(headers: &reqwest::header::HeaderMap, name: &str) -> Option<T>
where
    T: std::str::FromStr,
{
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse().ok())
}

fn optional_number<T: std::fmt::Display>(value: Option<T>) -> String {
    value.map_or_else(|| "unknown".to_owned(), |value| value.to_string())
}

pub(super) fn map_status_error(
    status: StatusCode,
    method: String,
    path: String,
    body: &[u8],
    token: &str,
    rate_limit: RateLimitHeaders,
) -> HackmdError {
    let body_detail = bounded_body(body, token);
    match status {
        StatusCode::UNAUTHORIZED => HackmdError::Unauthorized { method, path },
        StatusCode::FORBIDDEN => HackmdError::Forbidden { method, path },
        StatusCode::NOT_FOUND => HackmdError::NotFound { method, path },
        StatusCode::CONFLICT => HackmdError::Conflict { method, path },
        StatusCode::TOO_MANY_REQUESTS => HackmdError::RateLimited {
            method,
            path,
            detail: combine_details(rate_limit.detail(), &body_detail),
        },
        status if status.is_server_error() => HackmdError::Upstream {
            method,
            path,
            status,
            detail: nonempty_detail(body_detail),
        },
        status => HackmdError::Api {
            method,
            path,
            status,
            detail: nonempty_detail(body_detail),
        },
    }
}

fn combine_details(mut primary: String, secondary: &str) -> String {
    if !secondary.is_empty() {
        primary.push_str(": ");
        primary.push_str(secondary);
    }
    primary
}

fn nonempty_detail(detail: String) -> String {
    if detail.is_empty() {
        "no response detail".to_owned()
    } else {
        detail
    }
}

fn bounded_body(body: &[u8], token: &str) -> String {
    const MAX_CHARS: usize = 300;

    // Only the start of the body can be shown, so only the start is decoded: an
    // error body can be as large as the response cap. The cut leaves room for
    // the longest UTF-8 encoding of every shown character plus a whole token,
    // so a token that begins within them is still found and replaced.
    let cut = body.len().min(MAX_CHARS * 4 + token.len());
    let truncated = cut < body.len();
    let mut text = String::from_utf8_lossy(&body[..cut]).replace(token, "[REDACTED]");

    // Each replacement shortens the text, which can pull a token split by the
    // cut into the characters shown. Whatever prefix of it the cut left at the
    // end goes.
    if truncated
        && let Some(partial) = token
            .char_indices()
            .rev()
            .map(|(end, _)| &token[..end])
            .find(|prefix| !prefix.is_empty() && text.ends_with(prefix))
    {
        text.truncate(text.len() - partial.len());
    }
    let mut chars = text.chars();
    let mut bounded: String = chars.by_ref().take(MAX_CHARS).collect();
    if truncated || chars.next().is_some() {
        bounded.push('…');
    }
    bounded
}

#[derive(Debug, Error)]
pub(crate) enum HackmdError {
    #[error(
        "{method} {path}: HACKMD_API_TOKEN is not configured; set it in the server environment and restart the MCP server"
    )]
    MissingToken { method: String, path: String },
    #[error("failed to build the HackMD HTTP client")]
    ClientBuild,
    #[error("failed to serialize a validated HackMD request payload")]
    InvalidPayload,
    #[error("configured HACKMD_API_URL cannot be used as an API base URL")]
    InvalidBaseUrl,
    #[error("HackMD API path segments such as note IDs and team paths must not be empty")]
    EmptyPathSegment,
    #[error("HackMD API path segments such as note IDs and team paths must not be . or ..")]
    DotPathSegment,
    #[error("{method} {path}: HackMD response is larger than {limit_mib} MiB and was not read")]
    ResponseTooLarge {
        method: String,
        path: String,
        limit_mib: usize,
    },
    #[error(
        "POST {path}: HackMD rejected the image as too large (413); resize it below 5 MB and retry"
    )]
    ImageTooLarge { path: String },
    #[error("team workspace {team_path:?} is not available to this HackMD account")]
    UnknownTeam { team_path: String },
    #[error("{method} {path}: request timed out; check network connectivity and retry")]
    Timeout { method: String, path: String },
    #[error("HackMD write read-back exceeded its bounded verification window")]
    ReadbackTimeout,
    #[error("HackMD accepted the update for note {note_id}, but read-back content did not match")]
    ReadbackMismatch { note_id: String },
    #[error("remote note {note_id} has no Markdown content")]
    MissingContent { note_id: String },
    #[error("{method} {path}: network request failed; check connectivity and HACKMD_API_URL")]
    Network { method: String, path: String },
    #[error(
        "{method} {path}: 401 unauthorized; verify HACKMD_API_TOKEN and restart the MCP server"
    )]
    Unauthorized { method: String, path: String },
    #[error("{method} {path}: 403 forbidden; verify note/team permissions for this token")]
    Forbidden { method: String, path: String },
    #[error("{method} {path}: 404 not found; verify the note ID and workspace team_path")]
    NotFound { method: String, path: String },
    #[error("{method} {path}: 409 conflict; the requested permalink may already be in use")]
    Conflict { method: String, path: String },
    #[error("{method} {path}: 429 rate limited ({detail}); wait before retrying")]
    RateLimited {
        method: String,
        path: String,
        detail: String,
    },
    #[error("{method} {path}: upstream HackMD error ({status}): {detail}; retry later")]
    Upstream {
        method: String,
        path: String,
        status: StatusCode,
        detail: String,
    },
    #[error("{method} {path}: HackMD API error ({status}): {detail}")]
    Api {
        method: String,
        path: String,
        status: StatusCode,
        detail: String,
    },
    #[error("{method} {path}: HackMD returned invalid JSON in a {status} response")]
    InvalidJson {
        method: String,
        path: String,
        status: StatusCode,
    },
    #[error("{method} {path}: HackMD returned an empty response where JSON was required")]
    EmptyResponse { method: String, path: String },
}

impl crate::reply::ToolError for HackmdError {
    fn kind(&self) -> crate::reply::ErrorKind {
        use crate::reply::ErrorKind;

        match self {
            Self::MissingToken { .. } | Self::Unauthorized { .. } => ErrorKind::Auth,
            Self::Forbidden { .. } => ErrorKind::Forbidden,
            Self::NotFound { .. } | Self::UnknownTeam { .. } => ErrorKind::NotFound,
            Self::Conflict { .. } => ErrorKind::Conflict,
            Self::RateLimited { .. } => ErrorKind::RateLimited,
            Self::Timeout { .. } | Self::Network { .. } => ErrorKind::Network,
            Self::Upstream { .. } | Self::InvalidJson { .. } | Self::EmptyResponse { .. } => {
                ErrorKind::Upstream
            }
            Self::Api { .. } => ErrorKind::Api,
            Self::ReadbackTimeout | Self::ReadbackMismatch { .. } => ErrorKind::Readback,
            Self::MissingContent { .. } => ErrorKind::Upstream,
            Self::ResponseTooLarge { .. } | Self::ImageTooLarge { .. } => ErrorKind::TooLarge,
            Self::InvalidPayload | Self::EmptyPathSegment | Self::DotPathSegment => {
                ErrorKind::InvalidInput
            }
            Self::ClientBuild | Self::InvalidBaseUrl => ErrorKind::Internal,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::HackmdError;

    #[test]
    fn api_errors_convert_to_caller_visible_tool_errors() {
        let result = crate::reply::error(&HackmdError::Unauthorized {
            method: "GET".to_owned(),
            path: "/v1/me".to_owned(),
        });

        assert_eq!(result.is_error, Some(true));
        assert!(
            result.content[0]
                .as_text()
                .expect("tool error should contain text")
                .text
                .contains("401 unauthorized")
        );
        assert_eq!(
            result.meta.expect("errors carry _meta").0.get("error_kind"),
            Some(&serde_json::json!("auth"))
        );
    }

    #[test]
    fn body_errors_keep_their_kinds() {
        use crate::reply::{ErrorKind, ToolError};

        let note_id = "n".to_owned();
        assert_eq!(
            HackmdError::MissingContent {
                note_id: note_id.clone()
            }
            .kind(),
            ErrorKind::Upstream
        );
        assert_eq!(
            HackmdError::ReadbackMismatch { note_id }.kind(),
            ErrorKind::Readback
        );
    }

    #[test]
    fn a_token_split_by_the_cut_is_not_shown() {
        // Each redaction shortens the text, so without care the start of the
        // token the cut split lands inside the characters shown.
        let token = "t".repeat(40) + &"k".repeat(24);
        let body = token.repeat(40);
        let shown = super::bounded_body(body.as_bytes(), &token);
        assert!(!shown.contains('t') && !shown.contains('k'), "{shown}");
        assert!(shown.starts_with("[REDACTED][REDACTED]"));
        assert!(shown.ends_with('…'));
    }
}
