//! How a failed request is reported: the error type every tool surfaces, and
//! the mapping from status codes and rate-limit headers onto it.

use reqwest::StatusCode;
use thiserror::Error;

pub(super) fn request_error(error: &reqwest::Error, method: String, path: String) -> HackmdError {
    transport_error(error.is_timeout(), error.is_connect(), method, path)
}

/// A request that got no usable answer. A read can always be asked again. A
/// write that failed after connecting may already have reached `HackMD`, and
/// repeating a POST would create a second note: the caller is told to look
/// before sending it again.
pub(super) fn transport_error(
    timed_out: bool,
    before_connecting: bool,
    method: String,
    path: String,
) -> HackmdError {
    match (method != "GET" && !before_connecting, timed_out) {
        (true, timed_out) => HackmdError::WriteUnconfirmed {
            method,
            path,
            cause: if timed_out {
                "timed out"
            } else {
                "lost its connection"
            }
            .to_owned(),
        },
        (false, true) => HackmdError::Timeout { method, path },
        (false, false) => HackmdError::Network { method, path },
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

pub(super) fn parse_header<T>(headers: &reqwest::header::HeaderMap, name: &str) -> Option<T>
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

        // Only a note's permalink is known to clash; elsewhere HackMD's own
        // words are the best hint there is.
        StatusCode::CONFLICT => HackmdError::Conflict {
            detail: if is_note_route(&path) {
                "the requested permalink may already be in use".to_owned()
            } else {
                nonempty_detail(body_detail)
            },
            method,
            path,
        },
        StatusCode::TOO_MANY_REQUESTS => HackmdError::RateLimited {
            // A note PATCH is never retried for the caller (its body was read
            // just before), so say what the retry must start with. Create,
            // delete, and image upload have no body read to redo.
            then: if method == "PATCH" && is_note_route(&path) {
                ", then read the note again before writing it"
            } else {
                ""
            },
            method,
            path,
            detail: combine_details(rate_limit.detail(), &body_detail),
        },
        // A gateway can fail a write HackMD already applied.
        status if status.is_server_error() && method != "GET" => HackmdError::WriteUnconfirmed {
            method,
            path,
            cause: format!("HackMD answered {status}: {}", nonempty_detail(body_detail)),
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

/// Whether `path` names a note route, personal or team: the resource after
/// any `teams/{team_path}` prefix, so a team whose path is `notes` does not
/// count.
fn is_note_route(path: &str) -> bool {
    let mut segments = path.split('/').filter(|segment| !segment.is_empty());
    while let Some(segment) = segments.next() {
        match segment {
            "teams" => {
                segments.next();
            }
            "notes" => return true,
            "folders" | "trash" | "history" | "me" => return false,
            _ => {}
        }
    }
    false
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

#[derive(Debug, Clone, Error)]
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
    #[error(
        "HackMD API path segments such as note IDs and team paths must not contain control characters"
    )]
    ControlInPathSegment,
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
    #[error(
        "HackMD accepted the write, but reading it back did not finish in time; read it again and compare before writing again"
    )]
    ReadbackTimeout,
    #[error(
        "HackMD accepted the update for note {note_id}, but no read-back showed it; call hackmd_get_note and compare before writing again"
    )]
    ReadbackMismatch { note_id: String },
    #[error(
        "{method} {path}: the write {cause}; it may still have landed, so check whether it did before sending it again"
    )]
    WriteUnconfirmed {
        method: String,
        path: String,
        cause: String,
    },
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
    #[error("{method} {path}: 409 conflict; {detail}")]
    Conflict {
        method: String,
        path: String,
        detail: String,
    },
    #[error("{method} {path}: 429 rate limited ({detail}); wait before retrying{then}")]
    RateLimited {
        method: String,
        path: String,
        detail: String,
        then: &'static str,
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

impl HackmdError {
    /// A success reply that could not be read: invalid JSON (`status`), or
    /// an empty body (`None`). After a write this is no upstream fault to
    /// retry: the write landed, and repeating a create would duplicate it, so
    /// the agent is told to look first.
    pub(crate) fn unreadable_reply(
        method: String,
        path: String,
        status: Option<StatusCode>,
    ) -> Self {
        if method == "GET" {
            return match status {
                Some(status) => Self::InvalidJson {
                    method,
                    path,
                    status,
                },
                None => Self::EmptyResponse { method, path },
            };
        }
        let cause = status.map_or_else(
            || "was answered with an empty body where JSON was required".to_owned(),
            |status| format!("was answered {status} with invalid JSON"),
        );
        Self::WriteUnconfirmed {
            method,
            path,
            cause,
        }
    }
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
            Self::ReadbackTimeout
            | Self::ReadbackMismatch { .. }
            | Self::WriteUnconfirmed { .. } => ErrorKind::Readback,
            Self::MissingContent { .. } => ErrorKind::Upstream,
            Self::ResponseTooLarge { .. } | Self::ImageTooLarge { .. } => ErrorKind::TooLarge,
            Self::EmptyPathSegment | Self::DotPathSegment | Self::ControlInPathSegment => {
                ErrorKind::InvalidInput
            }

            // Serializing a payload this server validated cannot fail on
            // anything the caller sent.
            Self::ClientBuild | Self::InvalidBaseUrl | Self::InvalidPayload => ErrorKind::Internal,
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

    /// Kind names are a contract: this pins every variant's kind, so a change
    /// to the mapping is a visible decision rather than a side effect.
    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "one row per variant is the point: the table is the contract"
    )]
    fn every_variant_keeps_its_kind() {
        use crate::reply::{ErrorKind, ToolError};
        use reqwest::StatusCode;

        let m = || "GET".to_owned();
        let p = || "/v1/x".to_owned();
        let d = || "detail".to_owned();
        let cases = [
            (
                HackmdError::MissingToken {
                    method: m(),
                    path: p(),
                },
                ErrorKind::Auth,
            ),
            (HackmdError::ClientBuild, ErrorKind::Internal),
            (HackmdError::InvalidPayload, ErrorKind::Internal),
            (HackmdError::InvalidBaseUrl, ErrorKind::Internal),
            (HackmdError::EmptyPathSegment, ErrorKind::InvalidInput),
            (HackmdError::DotPathSegment, ErrorKind::InvalidInput),
            (HackmdError::ControlInPathSegment, ErrorKind::InvalidInput),
            (
                HackmdError::ResponseTooLarge {
                    method: m(),
                    path: p(),
                    limit_mib: 1,
                },
                ErrorKind::TooLarge,
            ),
            (
                HackmdError::ImageTooLarge { path: p() },
                ErrorKind::TooLarge,
            ),
            (
                HackmdError::UnknownTeam { team_path: d() },
                ErrorKind::NotFound,
            ),
            (
                HackmdError::Timeout {
                    method: m(),
                    path: p(),
                },
                ErrorKind::Network,
            ),
            (HackmdError::ReadbackTimeout, ErrorKind::Readback),
            (
                HackmdError::ReadbackMismatch { note_id: d() },
                ErrorKind::Readback,
            ),
            (
                HackmdError::WriteUnconfirmed {
                    method: m(),
                    path: p(),
                    cause: d(),
                },
                ErrorKind::Readback,
            ),
            (
                HackmdError::MissingContent { note_id: d() },
                ErrorKind::Upstream,
            ),
            (
                HackmdError::Network {
                    method: m(),
                    path: p(),
                },
                ErrorKind::Network,
            ),
            (
                HackmdError::Unauthorized {
                    method: m(),
                    path: p(),
                },
                ErrorKind::Auth,
            ),
            (
                HackmdError::Forbidden {
                    method: m(),
                    path: p(),
                },
                ErrorKind::Forbidden,
            ),
            (
                HackmdError::NotFound {
                    method: m(),
                    path: p(),
                },
                ErrorKind::NotFound,
            ),
            (
                HackmdError::Conflict {
                    method: m(),
                    path: p(),
                    detail: d(),
                },
                ErrorKind::Conflict,
            ),
            (
                HackmdError::RateLimited {
                    method: m(),
                    path: p(),
                    detail: d(),
                    then: "",
                },
                ErrorKind::RateLimited,
            ),
            (
                HackmdError::Upstream {
                    method: m(),
                    path: p(),
                    status: StatusCode::BAD_GATEWAY,
                    detail: d(),
                },
                ErrorKind::Upstream,
            ),
            (
                HackmdError::Api {
                    method: m(),
                    path: p(),
                    status: StatusCode::BAD_REQUEST,
                    detail: d(),
                },
                ErrorKind::Api,
            ),
            (
                HackmdError::InvalidJson {
                    method: m(),
                    path: p(),
                    status: StatusCode::OK,
                },
                ErrorKind::Upstream,
            ),
            (
                HackmdError::EmptyResponse {
                    method: m(),
                    path: p(),
                },
                ErrorKind::Upstream,
            ),
        ];
        for (error, kind) in cases {
            assert_eq!(error.kind(), kind, "{error}");
        }
    }

    #[test]
    fn an_unreadable_reply_to_a_write_says_look_not_retry() {
        use crate::reply::{ErrorKind, ToolError as _};
        use reqwest::StatusCode;

        for status in [Some(StatusCode::CREATED), None] {
            let read =
                HackmdError::unreadable_reply("GET".to_owned(), "/v1/notes".to_owned(), status);
            assert_eq!(read.kind(), ErrorKind::Upstream, "{read}");
            let write =
                HackmdError::unreadable_reply("POST".to_owned(), "/v1/notes".to_owned(), status);
            assert_eq!(write.kind(), ErrorKind::Readback, "{write}");
            assert!(
                write.to_string().contains("check whether it did"),
                "{write}"
            );
        }
    }

    #[test]
    fn a_409_names_the_permalink_only_on_note_routes() {
        let conflict = |path: &str| {
            super::map_status_error(
                reqwest::StatusCode::CONFLICT,
                "PATCH".to_owned(),
                path.to_owned(),
                b"folder name taken",
                "token",
                super::RateLimitHeaders::default(),
            )
            .to_string()
        };
        assert!(
            conflict("/v1/teams/t/notes/n")
                .ends_with("the requested permalink may already be in use")
        );
        assert!(conflict("/v1/teams/t/folders/f").ends_with("folder name taken"));
        assert!(conflict("/v1/teams/notes/folders/f").ends_with("folder name taken"));
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
