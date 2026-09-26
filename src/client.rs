use reqwest::{Method, StatusCode};
use serde::{
    Serialize,
    de::{DeserializeOwned, IgnoredAny},
};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use url::Url;

use crate::{
    config::Config,
    dto::{
        CreateFolderRequest, CreateNoteRequest, FolderResponse, HistoryResponse,
        ImageUploadResponse, NoteResponse, ProfileResponse, TeamResponse, UpdateFolderRequest,
        UpdateNoteRequest,
    },
    models::Workspace,
};

mod cache;
mod error;
mod readback;

use cache::{AccountCache, CacheFill, CacheLookup, NotesCache, fresh, store};
pub(crate) use error::HackmdError;
use error::{RateLimitHeaders, map_status_error, request_error};
pub(crate) use readback::Readback;
use readback::poll_readback_sized;

/// HTTP client shared by all `HackMD` tool handlers.
#[derive(Debug)]
pub(crate) struct HackmdClient {
    config: Config,
    http: reqwest::Client,
    notes: NotesCache,
    account: AccountCache,
}

impl HackmdClient {
    pub(crate) fn new(config: Config) -> Result<Self, HackmdError> {
        let http = reqwest::Client::builder()
            .connect_timeout(config.connect_timeout())
            .timeout(config.request_timeout())
            .build()
            .map_err(|_| HackmdError::ClientBuild)?;
        let notes = NotesCache::new(config.list_cache_ttl());
        let account = AccountCache::new(config.list_cache_ttl());
        Ok(Self {
            config,
            http,
            notes,
            account,
        })
    }

    pub(crate) fn has_api_token(&self) -> bool {
        self.config.has_api_token()
    }

    /// Always asks `HackMD`, and refreshes the cached `userPath` on the way,
    /// along with the team list when `/me` carries one.
    pub(crate) async fn get_me(&self) -> Result<ProfileResponse, HackmdError> {
        let profile: ProfileResponse = self.get_required(&["me"]).await?;
        store(
            &self.account.user_path,
            Arc::from(profile.user_path.as_str()),
        );
        if !profile.teams.is_empty() {
            store(&self.account.teams, Arc::from(profile.teams.as_slice()));
        }
        Ok(profile)
    }

    /// The authenticated `userPath`, from cache when fresh and `refresh` is
    /// false.
    pub(crate) async fn user_path(&self, refresh: bool) -> Result<Arc<str>, HackmdError> {
        if !refresh && let Some(user_path) = fresh(&self.account.user_path, self.account.ttl) {
            return Ok(user_path);
        }
        Ok(Arc::from(self.get_me().await?.user_path.as_str()))
    }

    /// The account's teams, from cache when fresh and `refresh` is false.
    pub(crate) async fn list_teams(
        &self,
        refresh: bool,
    ) -> Result<Arc<[TeamResponse]>, HackmdError> {
        if !refresh && let Some(teams) = fresh(&self.account.teams, self.account.ttl) {
            return Ok(teams);
        }
        let teams: Vec<TeamResponse> = self.get_required(&["teams"]).await?;
        let teams = Arc::<[TeamResponse]>::from(teams);
        store(&self.account.teams, Arc::clone(&teams));
        Ok(teams)
    }

    /// Whether the account belongs to `team_path`, by the same cache rules as
    /// `list_teams`.
    pub(crate) async fn has_team(
        &self,
        team_path: &str,
        refresh: bool,
    ) -> Result<bool, HackmdError> {
        Ok(includes_team(&self.list_teams(refresh).await?, team_path))
    }

    pub(crate) async fn ensure_team_exists(
        &self,
        workspace: &Workspace,
    ) -> Result<(), HackmdError> {
        let Workspace::Team { team_path } = workspace else {
            return Ok(());
        };

        // A write into a team the cache has not seen yet gets one fresh look
        // before being refused, so joining a team takes effect immediately. A
        // cold cache is filled by that same look, never fetched twice.
        let cached = fresh(&self.account.teams, self.account.ttl)
            .is_some_and(|teams| includes_team(&teams, team_path));
        if cached || self.has_team(team_path, true).await? {
            Ok(())
        } else {
            Err(HackmdError::UnknownTeam {
                team_path: team_path.clone(),
            })
        }
    }

    pub(crate) async fn get_history(&self) -> Result<Vec<NoteResponse>, HackmdError> {
        self.get_required::<HistoryResponse>(&["history"])
            .await
            .map(HistoryResponse::into_notes)
    }

    pub(crate) async fn list_trash(&self) -> Result<Vec<NoteResponse>, HackmdError> {
        self.get_required(&["trash"]).await
    }

    /// Any response body is ignored: success is the status code.
    pub(crate) async fn restore_note(&self, note_id: &str) -> Result<(), HackmdError> {
        self.request_json_idempotent::<IgnoredAny>(
            Method::PUT,
            &["trash", note_id, "restore"],
            NO_BODY,
        )
        .await
        .map(drop)
    }

    /// Lists a workspace's notes, from cache when one is still fresh.
    ///
    /// Resolving a `hackmd.io/@owner/slug` reference lists the whole workspace,
    /// so an agent working through URLs pays for the same list repeatedly. Any
    /// note write clears the cache, and the window is short, but a caller that
    /// must see another client's change immediately should not rely on this.
    pub(crate) async fn list_notes(
        &self,
        workspace: &Workspace,
        refresh: bool,
    ) -> Result<Arc<[NoteResponse]>, HackmdError> {
        let mut bypass_cache = refresh;
        loop {
            match self.notes.begin(workspace, bypass_cache) {
                CacheLookup::Hit(notes) => return Ok(notes),
                CacheLookup::Wait(flight) => {
                    flight.wait().await;
                    bypass_cache = false;
                }
                CacheLookup::Fill { generation, flight } => {
                    let fill = CacheFill::new(&self.notes, workspace.clone(), generation, flight);
                    let notes: Arc<[NoteResponse]> = self
                        .get_required::<Vec<NoteResponse>>(&workspace_route(workspace, &["notes"]))
                        .await?
                        .into();
                    if fill.complete(&notes) {
                        return Ok(notes);
                    }
                    bypass_cache = false;
                }
            }
        }
    }

    pub(crate) async fn get_note(
        &self,
        workspace: &Workspace,
        note_id: &str,
    ) -> Result<NoteResponse, HackmdError> {
        self.get_required(&workspace_route(workspace, &["notes", note_id]))
            .await
    }

    pub(crate) async fn create_note(
        &self,
        workspace: &Workspace,
        payload: &CreateNoteRequest,
    ) -> Result<NoteResponse, HackmdError> {
        let segments = workspace_route(workspace, &["notes"]);
        self.request_required(Method::POST, &segments, Some(payload))
            .await
    }

    pub(crate) async fn update_note(
        &self,
        workspace: &Workspace,
        note_id: &str,
        payload: &UpdateNoteRequest,
    ) -> Result<Option<NoteResponse>, HackmdError> {
        let segments = workspace_route(workspace, &["notes", note_id]);
        self.request_json_idempotent(Method::PATCH, &segments, Some(payload))
            .await
    }

    /// Updates only note content without cloning the caller's potentially
    /// large Markdown body into an owned DTO before JSON encoding.
    pub(crate) async fn update_note_content(
        &self,
        workspace: &Workspace,
        note_id: &str,
        content: &str,
    ) -> Result<Option<Value>, HackmdError> {
        #[derive(Serialize)]
        struct ContentUpdate<'a> {
            content: &'a str,
        }

        let segments = workspace_route(workspace, &["notes", note_id]);
        self.request_json_idempotent(Method::PATCH, &segments, Some(&ContentUpdate { content }))
            .await
    }

    pub(crate) async fn delete_note(
        &self,
        workspace: &Workspace,
        note_id: &str,
    ) -> Result<(), HackmdError> {
        let segments = workspace_route(workspace, &["notes", note_id]);
        self.request_json::<IgnoredAny>(Method::DELETE, &segments, NO_BODY)
            .await
            .map(drop)
    }

    pub(crate) async fn list_folders(
        &self,
        workspace: &Workspace,
    ) -> Result<Vec<FolderResponse>, HackmdError> {
        self.get_required(&workspace_route(workspace, &["folders"]))
            .await
    }

    pub(crate) async fn get_folder(
        &self,
        workspace: &Workspace,
        folder_id: &str,
    ) -> Result<FolderResponse, HackmdError> {
        self.get_required(&workspace_route(workspace, &["folders", folder_id]))
            .await
    }

    pub(crate) async fn create_folder(
        &self,
        workspace: &Workspace,
        payload: &CreateFolderRequest,
    ) -> Result<FolderResponse, HackmdError> {
        let segments = workspace_route(workspace, &["folders"]);
        self.request_required(Method::POST, &segments, Some(payload))
            .await
    }

    pub(crate) async fn update_folder(
        &self,
        workspace: &Workspace,
        folder_id: &str,
        payload: &UpdateFolderRequest,
    ) -> Result<Option<FolderResponse>, HackmdError> {
        let segments = workspace_route(workspace, &["folders", folder_id]);
        self.request_json_idempotent(Method::PATCH, &segments, Some(payload))
            .await
    }

    pub(crate) async fn delete_folder(
        &self,
        workspace: &Workspace,
        folder_id: &str,
    ) -> Result<Option<Value>, HackmdError> {
        let segments = workspace_route(workspace, &["folders", folder_id]);
        self.request_json(Method::DELETE, &segments, NO_BODY).await
    }

    pub(crate) async fn get_folder_order(
        &self,
        workspace: &Workspace,
    ) -> Result<BTreeMap<String, Vec<String>>, HackmdError> {
        self.get_required(&workspace_route(workspace, &["folders", "folder-order"]))
            .await
    }

    pub(crate) async fn set_folder_order(
        &self,
        workspace: &Workspace,
        order: &BTreeMap<String, Vec<String>>,
    ) -> Result<Option<Value>, HackmdError> {
        let segments = workspace_route(workspace, &["folders", "folder-order"]);
        self.request_json(Method::PUT, &segments, Some(&json!({"order": order})))
            .await
    }

    pub(crate) async fn upload_note_image(
        &self,
        note_id: &str,
        file_name: &str,
        image: tokio::fs::File,
        size_bytes: u64,
    ) -> Result<ImageUploadResponse, HackmdError> {
        let segments = ["notes", note_id, "images"];
        let url = self.url_for_segments(&segments)?;
        let path = url.path().to_owned();
        let token = self
            .config
            .api_token()
            .ok_or_else(|| HackmdError::MissingToken {
                method: "POST".to_owned(),
                path: path.clone(),
            })?;
        let form = reqwest::multipart::Form::new().part(
            "image",
            reqwest::multipart::Part::stream_with_length(image, size_bytes)
                .file_name(file_name.to_owned()),
        );
        tracing::debug!(method = "POST", path = %path, "HackMD request started");
        let response = self
            .http
            .post(url)
            .bearer_auth(token)
            .multipart(form)
            .send()
            .await
            .map_err(|error| request_error(&error, "POST".to_owned(), path.clone()))?;
        let status = response.status();
        let rate_limit = RateLimitHeaders::from_headers(response.headers());
        let bytes = read_body_capped(response, RESPONSE_MAX_BYTES)
            .await
            .map_err(|error| error.into_hackmd("POST", &path))?;
        tracing::debug!(
            method = "POST",
            path = %path,
            status = status.as_u16(),
            retries = 0,
            "HackMD request completed"
        );
        if status == StatusCode::PAYLOAD_TOO_LARGE {
            return Err(HackmdError::ImageTooLarge { path });
        }
        decode_response(
            status,
            "POST".to_owned(),
            path.clone(),
            &bytes,
            token,
            rate_limit,
        )?
        .ok_or(HackmdError::EmptyResponse {
            method: "POST".to_owned(),
            path,
        })
    }

    /// Issues a GET whose response body is mandatory.
    async fn get_required<T: DeserializeOwned>(&self, segments: &[&str]) -> Result<T, HackmdError> {
        self.request_required(Method::GET, segments, NO_BODY).await
    }

    /// Issues a request that must answer with a JSON body, turning `HackMD`'s
    /// empty-body success responses into an error instead of a silent `None`.
    async fn request_required<T: DeserializeOwned>(
        &self,
        method: Method,
        segments: &[&str],
        body: Option<&(impl Serialize + Sync)>,
    ) -> Result<T, HackmdError> {
        match self.request_json(method.clone(), segments, body).await? {
            Some(value) => Ok(value),
            None => Err(HackmdError::EmptyResponse {
                method: method.as_str().to_owned(),
                path: self.url_for_segments(segments)?.path().to_owned(),
            }),
        }
    }

    /// Sends a request, retrying only reads. A POST or DELETE that fails
    /// ambiguously is left to the caller, because repeating it could create a
    /// second note or delete something the first attempt already removed.
    async fn request_json<T: DeserializeOwned>(
        &self,
        method: Method,
        path_segments: &[&str],
        body: Option<&(impl Serialize + Sync)>,
    ) -> Result<Option<T>, HackmdError> {
        let retryable = method == Method::GET;
        self.request_json_with_retry(method, path_segments, body, retryable)
            .await
    }

    /// Sends a request that may be retried because repeating it lands on the
    /// same state: `HackMD`'s PATCH and PUT routes set fields to given values
    /// rather than accumulating. A retry can still overwrite an edit made by
    /// someone else in between, which is the same exposure the first attempt
    /// already had.
    async fn request_json_idempotent<T: DeserializeOwned>(
        &self,
        method: Method,
        path_segments: &[&str],
        body: Option<&(impl Serialize + Sync)>,
    ) -> Result<Option<T>, HackmdError> {
        self.request_json_with_retry(method, path_segments, body, true)
            .await
    }

    /// Drops every cached note list around a request that is not a read:
    /// once now and once more when the returned guard is dropped, which is
    /// every way out of the request, error paths included. The first covers
    /// a write that fails with a timeout yet still landed on `HackMD`; the
    /// second covers a concurrent list that refilled the cache while the
    /// write was in flight. Both live here rather than at each write site,
    /// where the next endpoint added would be free to forget.
    fn invalidate_list_cache_on_write(&self, method: &Method) -> Option<InvalidateOnDrop<'_>> {
        (method != Method::GET).then(|| {
            self.notes.invalidate();
            InvalidateOnDrop(&self.notes)
        })
    }

    /// `poll_readback` for a write this client made, sized to the body that
    /// was written. A confirmed read-back also drops cached note lists once
    /// more: one fetched while the write was still settling may hold the old
    /// title or permalink.
    pub(crate) async fn poll_readback<T, Fut>(
        &self,
        written_bytes: usize,
        fetch: impl FnMut() -> Fut,
        accepted: impl Fn(&T) -> bool,
    ) -> Result<Readback<T>, HackmdError>
    where
        Fut: std::future::Future<Output = Result<T, HackmdError>>,
    {
        let readback = poll_readback_sized(written_bytes, fetch, accepted).await?;
        if readback.confirmed {
            self.notes.invalidate();
        }
        Ok(readback)
    }

    async fn request_json_with_retry<T: DeserializeOwned>(
        &self,
        method: Method,
        path_segments: &[&str],
        body: Option<&(impl Serialize + Sync)>,
        retryable: bool,
    ) -> Result<Option<T>, HackmdError> {
        let url = self.url_for_segments(path_segments)?;
        let path = url.path().to_owned();
        let method_text = method.as_str().to_owned();
        let token = self
            .config
            .api_token()
            .ok_or_else(|| HackmdError::MissingToken {
                method: method_text.clone(),
                path: path.clone(),
            })?;
        let retry = self.config.retry();
        let mut retries = 0_u8;

        // Encoded once, not once per attempt: a retried PATCH carries the whole
        // note body, and re-encoding it costs more than copying the bytes.
        let body = body
            .map(|body| serde_json::to_vec(body))
            .transpose()
            .map_err(|_| HackmdError::InvalidPayload)?;

        let mut request = self.http.request(method.clone(), url).bearer_auth(token);
        if let Some(body) = body {
            request = request
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(body);
        }

        let _invalidate = self.invalidate_list_cache_on_write(&method);
        tracing::debug!(method = %method_text, path = %path, "HackMD request started");
        let (status, bytes, rate_limit) = loop {
            let request = request.try_clone().ok_or(HackmdError::InvalidPayload)?;
            let response = match request.send().await {
                Ok(response) => response,
                Err(_) if retryable && retries < retry.max_retries => {
                    sleep_before_retry(retries, None, retry, false).await;
                    retries += 1;
                    continue;
                }
                Err(error) => {
                    return Err(request_error(&error, method_text.clone(), path.clone()));
                }
            };
            let status = response.status();
            let retry_after = retry_after(response.headers());
            let rate_limit = RateLimitHeaders::from_headers(response.headers());
            let bytes = match read_body_capped(response, RESPONSE_MAX_BYTES).await {
                Ok(bytes) => bytes,
                Err(BodyError::Transport(_)) if retryable && retries < retry.max_retries => {
                    sleep_before_retry(retries, None, retry, false).await;
                    retries += 1;
                    continue;
                }
                Err(error) => return Err(error.into_hackmd(&method_text, &path)),
            };

            // A wait longer than the backoff cap is not waited out in capped
            // slices, which would only spend every retry inside the same
            // window: the rate-limit error goes back at once, naming the reset.
            let wait_too_long = retry_after.is_some_and(|wait| wait > retry.max_backoff);
            if retryable
                && retries < retry.max_retries
                && !wait_too_long
                && (status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error())
            {
                sleep_before_retry(
                    retries,
                    retry_after,
                    retry,
                    status == StatusCode::TOO_MANY_REQUESTS,
                )
                .await;
                retries += 1;
                continue;
            }
            break (status, bytes, rate_limit);
        };

        tracing::debug!(
            method = %method_text,
            path = %path,
            status = status.as_u16(),
            retries,
            "HackMD request completed"
        );

        decode_response(status, method_text, path, &bytes, token, rate_limit)
    }

    fn url_for_segments(&self, path_segments: &[&str]) -> Result<Url, HackmdError> {
        if path_segments
            .iter()
            .any(|segment| segment.trim().is_empty())
        {
            return Err(HackmdError::EmptyPathSegment);
        }

        // The URL library drops these rather than encoding them, so a note ID
        // of `..` would quietly turn `/teams/../notes/X` into another route on
        // the same API.
        if path_segments
            .iter()
            .any(|segment| matches!(*segment, "." | ".."))
        {
            return Err(HackmdError::DotPathSegment);
        }
        let mut url = self.config.api_url().clone();
        let mut segments = url
            .path_segments_mut()
            .map_err(|()| HackmdError::InvalidBaseUrl)?;
        segments.pop_if_empty();
        segments.extend(path_segments);
        drop(segments);
        Ok(url)
    }
}

/// Reads how long the server asked us to wait, preferring the standard header
/// and falling back to `HackMD`'s own rate-limit reset. A reset further out
/// than the maximum backoff is not retried at all: the rate-limit error is
/// returned straight away.
fn retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    if let Some(value) = headers
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
    {
        if let Ok(seconds) = value.parse::<u64>() {
            return Some(Duration::from_secs(seconds));
        }
        if let Ok(target) = httpdate::parse_http_date(value) {
            return target.duration_since(SystemTime::now()).ok();
        }
    }
    headers
        .get("x-ratelimit-userreset")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .map(Duration::from_secs)
}

async fn sleep_before_retry(
    retry_index: u8,
    retry_after: Option<Duration>,
    config: crate::config::RetryConfig,
    was_rate_limited: bool,
) {
    let exponential = config
        .initial_backoff
        .saturating_mul(2_u32.saturating_pow(u32::from(retry_index)))
        .min(config.max_backoff);
    let delay = retry_after.map_or_else(
        || {
            let max_nanos = u64::try_from(exponential.as_nanos()).unwrap_or(u64::MAX);
            Duration::from_nanos(fastrand::u64(0..=max_nanos))
        },
        |duration| duration.min(config.max_backoff),
    );
    crate::retry::record_retry(delay, was_rate_limited);
    tokio::time::sleep(delay).await;
}

/// The largest response body read into memory. Note bodies are capped at
/// 50 MiB, and JSON escaping of a Markdown body rarely comes near doubling
/// it; anything larger is refused before it is buffered, not after.
const RESPONSE_MAX_BYTES: usize = 2 * crate::sync::BODY_MAX_BYTES + 1024 * 1024;

/// Clears the note-list cache when dropped, on whichever path leaves the
/// request.
struct InvalidateOnDrop<'a>(&'a NotesCache);

impl Drop for InvalidateOnDrop<'_> {
    fn drop(&mut self) {
        self.0.invalidate();
    }
}

enum BodyError {
    Transport(reqwest::Error),
    TooLarge,
}

impl BodyError {
    fn into_hackmd(self, method: &str, path: &str) -> HackmdError {
        match self {
            Self::Transport(error) => request_error(&error, method.to_owned(), path.to_owned()),
            Self::TooLarge => HackmdError::ResponseTooLarge {
                method: method.to_owned(),
                path: path.to_owned(),
                limit_mib: RESPONSE_MAX_BYTES / 1024 / 1024,
            },
        }
    }
}

/// Reads a response body of at most `cap` bytes. A declared length over the
/// cap is refused before anything is read, and a body without one is
/// refused as soon as it passes the cap.
async fn read_body_capped(
    mut response: reqwest::Response,
    cap: usize,
) -> Result<Vec<u8>, BodyError> {
    let declared = response
        .content_length()
        .map(|length| usize::try_from(length).unwrap_or(usize::MAX));
    if declared.is_some_and(|length| length > cap) {
        return Err(BodyError::TooLarge);
    }
    let mut body = Vec::with_capacity(declared.unwrap_or(0));
    while let Some(chunk) = response.chunk().await.map_err(BodyError::Transport)? {
        if body.len() + chunk.len() > cap {
            return Err(BodyError::TooLarge);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// Turns a finished response into the caller's result: an error named by its
/// status, `None` for `HackMD`'s empty success bodies, or the decoded JSON.
fn decode_response<T: DeserializeOwned>(
    status: StatusCode,
    method: String,
    path: String,
    bytes: &[u8],
    token: &str,
    rate_limit: RateLimitHeaders,
) -> Result<Option<T>, HackmdError> {
    if !status.is_success() {
        return Err(map_status_error(
            status, method, path, bytes, token, rate_limit,
        ));
    }
    if bytes.is_empty() {
        return Ok(None);
    }
    serde_json::from_slice(bytes)
        .map(Some)
        .map_err(|_| HackmdError::InvalidJson {
            method,
            path,
            status,
        })
}

/// Names the absent body at the many call sites that have none: `None` alone
/// cannot infer the payload type once the parameter is generic.
const NO_BODY: Option<&Value> = None;

fn includes_team(teams: &[TeamResponse], team_path: &str) -> bool {
    teams.iter().any(|team| team.path == team_path)
}

/// Builds the path segments for a workspace-scoped route. Personal routes start
/// at the resource; the same resource for a team nests under its team path.
fn workspace_route<'a>(workspace: &'a Workspace, resource: &[&'a str]) -> Vec<&'a str> {
    match workspace {
        Workspace::Personal => resource.to_vec(),
        Workspace::Team { team_path } => {
            let mut segments = vec!["teams", team_path.as_str()];
            segments.extend_from_slice(resource);
            segments
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{fmt::Write as _, sync::Arc, time::Duration};

    const NOTES: &str = r#"[{"id":"note-id","title":"Note"}]"#;

    use reqwest::Method;
    use serde_json::{Value, json};

    use super::{HackmdClient, HackmdError, NO_BODY, retry_after};
    use crate::config::Config;

    #[test]
    fn retry_delay_uses_hackmd_reset_header_as_fallback() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            "x-ratelimit-userreset",
            reqwest::header::HeaderValue::from_static("4"),
        );
        assert_eq!(retry_after(&headers), Some(Duration::from_secs(4)));

        headers.insert(
            reqwest::header::RETRY_AFTER,
            reqwest::header::HeaderValue::from_static("2"),
        );
        assert_eq!(retry_after(&headers), Some(Duration::from_secs(2)));
    }
    use crate::{
        dto::{CreateNoteRequest, UpdateNoteRequest},
        fixture::{FIXTURE_TOKEN, Scenario, SequenceServer},
        models::Workspace,
    };

    #[tokio::test]
    async fn a_fresh_note_list_is_reused_until_a_write_invalidates_it() {
        // Two list responses for three list calls: the second call is served
        // from cache, and only the write in between forces another fetch.
        let server = SequenceServer::spawn_scenarios([
            Scenario::new("GET", "/v1/notes", 200, NOTES),
            Scenario::new("PATCH", "/v1/notes/note-id", 202, "")
                .expect_header("content-type", "application/json")
                .expect_body("the renamed title", |body| body == r#"{"title":"Renamed"}"#),
            Scenario::new("GET", "/v1/notes", 200, NOTES),
        ]);
        let client = server.client_with_cache();

        let first = client
            .list_notes(&Workspace::Personal, false)
            .await
            .expect("first list should fetch");
        let second = client
            .list_notes(&Workspace::Personal, false)
            .await
            .expect("second list should come from cache");
        assert_eq!(first, second);

        client
            .update_note(
                &Workspace::Personal,
                "note-id",
                &UpdateNoteRequest {
                    title: Some("Renamed".to_owned()),
                    ..UpdateNoteRequest::default()
                },
            )
            .await
            .expect("write should succeed");
        client
            .list_notes(&Workspace::Personal, false)
            .await
            .expect("list after a write should fetch again");

        server.finish();
    }

    #[tokio::test]
    async fn refresh_bypasses_and_replaces_a_cached_note_list() {
        const RENAMED: &str = r#"[{"id":"note-id","title":"Renamed"}]"#;
        let server = SequenceServer::spawn_scenarios([
            Scenario::new("GET", "/v1/notes", 200, NOTES),
            Scenario::new("GET", "/v1/notes", 200, RENAMED),
        ]);
        let client = server.client_with_cache();

        let original = client
            .list_notes(&Workspace::Personal, false)
            .await
            .expect("first list should fetch");
        let refreshed = client
            .list_notes(&Workspace::Personal, true)
            .await
            .expect("refresh should fetch");
        let cached = client
            .list_notes(&Workspace::Personal, false)
            .await
            .expect("refreshed list should be cached");

        assert_eq!(original[0].title, "Note");
        assert_eq!(refreshed[0].title, "Renamed");
        assert_eq!(cached, refreshed);
        server.finish();
    }

    #[tokio::test]
    async fn concurrent_workspace_misses_share_one_list_request() {
        let server =
            SequenceServer::spawn_scenarios([
                Scenario::new("GET", "/v1/notes", 200, NOTES).delay(Duration::from_millis(100))
            ]);
        let client = Arc::new(server.client_with_cache());

        let (first, second) = tokio::join!(
            client.list_notes(&Workspace::Personal, false),
            client.list_notes(&Workspace::Personal, false)
        );

        assert_eq!(first.expect("first list should succeed").len(), 1);
        assert_eq!(second.expect("second list should succeed").len(), 1);
        server.finish();
    }

    /// Manual, threshold-free baseline for the list hot path. Run with:
    /// `cargo test benchmark_10k_note_list_cache_and_filter -- --ignored
    /// --nocapture`.
    #[tokio::test]
    #[ignore = "manual allocation and latency baseline"]
    async fn benchmark_10k_note_list_cache_and_filter() {
        let mut body = String::with_capacity(1_500_000);
        body.push('[');
        for index in 0..10_000 {
            if index != 0 {
                body.push(',');
            }
            write!(
                body,
                r#"{{"id":"note-{index}","title":"Roadmap {index}","description":"Rust benchmark","tags":["rust"],"lastChangedAt":{index}}}"#
            )
            .expect("writing JSON into a String should succeed");
        }
        body.push(']');

        let server =
            SequenceServer::spawn_scenarios([
                Scenario::new("GET", "/v1/notes", 200, &body).delay(Duration::from_millis(25))
            ]);
        let client = Arc::new(server.client_with_cache());
        let miss_started = std::time::Instant::now();
        let mut callers = tokio::task::JoinSet::new();
        for _ in 0..16 {
            let client = Arc::clone(&client);
            callers.spawn(async move { client.list_notes(&Workspace::Personal, false).await });
        }
        while let Some(result) = callers.join_next().await {
            assert_eq!(
                result
                    .expect("benchmark caller should join")
                    .expect("benchmark list should succeed")
                    .len(),
                10_000
            );
        }
        let miss_elapsed = miss_started.elapsed();

        let filter_started = std::time::Instant::now();
        let page = crate::note::list::list_notes(
            &client,
            crate::note::list::ListNotesInput {
                source: crate::note::list::NoteSource::Workspace,
                workspace: Workspace::Personal,
                limit: 100,
                offset: 4_900,
                query: Some("roadmap".to_owned()),
                tags: vec!["RUST".to_owned()],
                refresh: false,
                sort: Some(crate::note::list::NoteSort::TitleAsc),
            },
        )
        .await
        .expect("benchmark filter should succeed");
        let filter_elapsed = filter_started.elapsed();
        assert_eq!(page.meta.total, 10_000);
        assert_eq!(page.meta.count, 100);

        let (hits, misses, retained_bytes) = client.notes.stats();
        eprintln!(
            "{{\"notes\":10000,\"concurrent_callers\":16,\"requests\":{},\"cache_hits\":{},\"cache_misses\":{},\"estimated_retained_bytes\":{},\"coalesced_miss_us\":{},\"cached_filter_sort_us\":{}}}",
            1,
            hits,
            misses,
            retained_bytes,
            miss_elapsed.as_micros(),
            filter_elapsed.as_micros()
        );
        server.finish();
    }

    #[tokio::test]
    async fn a_write_that_fails_still_drops_the_cached_list() {
        // HackMD applies writes asynchronously, so a PATCH that answers with an
        // error may still have landed. The cache has to go either way.
        let server = SequenceServer::spawn_scenarios([
            Scenario::new("GET", "/v1/notes", 200, NOTES),
            Scenario::new("PATCH", "/v1/notes/note-id", 400, r#"{"error":"nope"}"#),
            Scenario::new("GET", "/v1/notes", 200, NOTES),
        ]);
        let client = server.client_with_cache();

        client
            .list_notes(&Workspace::Personal, false)
            .await
            .expect("first list should fetch");
        client
            .update_note(
                &Workspace::Personal,
                "note-id",
                &UpdateNoteRequest {
                    title: Some("Renamed".to_owned()),
                    ..UpdateNoteRequest::default()
                },
            )
            .await
            .expect_err("the fixture rejects this write");
        client
            .list_notes(&Workspace::Personal, false)
            .await
            .expect("list after a failed write should fetch again");

        server.finish();
    }

    #[tokio::test]
    async fn team_and_personal_lists_do_not_share_a_cache_entry() {
        let server = SequenceServer::spawn_scenarios([
            Scenario::new("GET", "/v1/notes", 200, NOTES),
            Scenario::new("GET", "/v1/teams/core/notes", 200, NOTES),
        ]);
        let client = server.client_with_cache();

        client
            .list_notes(&Workspace::Personal, false)
            .await
            .expect("personal list should fetch");
        client
            .list_notes(
                &Workspace::Team {
                    team_path: "core".to_owned(),
                },
                false,
            )
            .await
            .expect("team list should fetch separately");

        server.finish();
    }

    #[tokio::test]
    async fn encodes_each_path_segment_and_attaches_bearer_auth() {
        const TOKEN: &str = "fixture-bearer-token";
        let server = SequenceServer::spawn_scenarios([Scenario::new(
            "GET",
            "/v1/teams/team%2Fpath/notes/note%20%3F%23",
            200,
            r#"{"id":"ok"}"#,
        )
        .expect_header("authorization", "Bearer fixture-bearer-token")]);
        let client = server.client_with_token(TOKEN);
        assert!(!format!("{client:?}").contains(TOKEN));

        let response = client
            .request_json::<Value>(
                Method::GET,
                &["teams", "team/path", "notes", "note ?#"],
                NO_BODY,
            )
            .await
            .expect("fixture request should succeed")
            .expect("JSON response should be present");
        assert_eq!(response, json!({"id": "ok"}));
        server.finish();
    }

    #[tokio::test]
    async fn sends_json_payload_and_parses_response_once() {
        let server = SequenceServer::spawn_scenarios([Scenario::new(
            "PATCH",
            "/v1/notes/id",
            200,
            r#"{"saved":true}"#,
        )
        .expect_header("content-type", "application/json")
        .expect_body("the title JSON", |body| body == r#"{"title":"hello"}"#)]);
        let client = server.client();
        let payload = json!({"title": "hello"});

        let response = client
            .request_json::<Value>(Method::PATCH, &["notes", "id"], Some(&payload))
            .await
            .expect("fixture request should succeed");
        assert_eq!(response, Some(json!({"saved": true})));
        server.finish();
    }

    #[tokio::test]
    async fn omitted_optional_fields_remain_omitted_on_the_wire() {
        let server = SequenceServer::spawn_scenarios([Scenario::new("POST", "/v1/notes", 204, "")
            .expect_header("content-type", "application/json")
            .expect_body("an empty JSON object", |body| body == "{}")]);
        let client = server.client();
        let payload = serde_json::to_value(CreateNoteRequest::default())
            .expect("typed payload should serialize");

        let response = client
            .request_json::<Value>(Method::POST, &["notes"], Some(&payload))
            .await
            .expect("fixture request should succeed");
        assert_eq!(response, None);
        server.finish();
    }

    #[tokio::test]
    async fn accepts_empty_202_and_204_responses() {
        for status in [202, 204] {
            let server = SequenceServer::spawn_scenarios([Scenario::new(
                "PATCH",
                "/v1/notes/id",
                status,
                "",
            )]);
            let client = server.client_without_retry(FIXTURE_TOKEN);
            let response = client
                .request_json::<Value>(Method::PATCH, &["notes", "id"], NO_BODY)
                .await
                .expect("empty success should be accepted");
            assert_eq!(response, None);
            server.finish();
        }
    }

    #[tokio::test]
    async fn typed_crud_operations_use_workspace_routes_and_payloads() {
        let created = r#"{"id":"new-id","title":"New"}"#;
        let create_server = SequenceServer::spawn_scenarios([Scenario::new(
            "POST",
            "/v1/teams/team%2Fpath/notes",
            201,
            created,
        )
        .expect_header("content-type", "application/json")
        .expect_body("the new title", |body| body == r#"{"title":"New"}"#)]);
        let create_client = create_server.client();
        let note = create_client
            .create_note(
                &Workspace::Team {
                    team_path: "team/path".to_owned(),
                },
                &CreateNoteRequest {
                    title: Some("New".to_owned()),
                    ..CreateNoteRequest::default()
                },
            )
            .await
            .expect("team note should be created");
        assert_eq!(note.id, "new-id");
        create_server.finish();

        let update_server = SequenceServer::spawn_scenarios([Scenario::new(
            "PATCH",
            "/v1/notes/note%2Fid",
            202,
            "",
        )
        .expect_header("content-type", "application/json")
        .expect_body("the cleared parent folder", |body| {
            body == r#"{"parentFolderId":null}"#
        })]);
        let update_client = update_server.client();
        let response = update_client
            .update_note(
                &Workspace::Personal,
                "note/id",
                &UpdateNoteRequest {
                    parent_folder_id: Some(None),
                    ..UpdateNoteRequest::default()
                },
            )
            .await
            .expect("personal note update should be accepted");
        assert_eq!(response, None);
        update_server.finish();

        let delete_server = SequenceServer::spawn_scenarios([Scenario::new(
            "DELETE",
            "/v1/teams/team/notes/note-id",
            204,
            "",
        )]);
        let delete_client = delete_server.client();
        delete_client
            .delete_note(
                &Workspace::Team {
                    team_path: "team".to_owned(),
                },
                "note-id",
            )
            .await
            .expect("team note should be deleted");
        delete_server.finish();
    }

    #[tokio::test]
    async fn maps_every_required_http_failure() {
        let cases = [
            (401, "401 unauthorized"),
            (403, "403 forbidden"),
            (404, "404 not found"),
            (409, "409 conflict"),
            (429, "429 rate limited"),
            (500, "upstream HackMD error (500 Internal Server Error)"),
        ];
        for (status, expected) in cases {
            let server = SequenceServer::spawn_scenarios([Scenario::new(
                "GET",
                "/v1/notes/id",
                status,
                r#"{"error":"fixture"}"#,
            )]);
            let client = server.client_without_retry(FIXTURE_TOKEN);
            let error = client
                .request_json::<Value>(Method::GET, &["notes", "id"], NO_BODY)
                .await
                .expect_err("failure status should map to an error");
            let message = error.to_string();
            assert!(message.starts_with("GET /v1/notes/id:"));
            assert!(message.contains(expected), "unexpected error: {message}");
            server.finish();
        }
    }

    #[tokio::test]
    async fn rate_limit_error_reports_hackmd_quota_headers() {
        const HEADERS: &[(&str, &str)] = &[
            ("x-ratelimit-userlimit", "100"),
            ("x-ratelimit-userremaining", "0"),
            ("x-ratelimit-userreset", "42"),
        ];
        let server = SequenceServer::spawn_scenarios([HEADERS.iter().fold(
            Scenario::new("GET", "/v1/notes", 429, r#"{"error":"slow down"}"#),
            |scenario, (name, value)| scenario.response_header(name, value),
        )]);
        let client = HackmdClient::new(Config::for_loopback_test_no_retry(
            &server.api_url,
            "fixture-token",
        ))
        .expect("fixture client should build");

        let message = client
            .request_json::<Value>(Method::GET, &["notes"], NO_BODY)
            .await
            .expect_err("429 should map to rate-limit detail")
            .to_string();
        assert!(message.contains("remaining 0/100, reset after 42 seconds"));
        assert!(message.contains(r#"{"error":"slow down"}"#));
        server.finish();
    }

    #[tokio::test]
    async fn upstream_errors_keep_bounded_redacted_body_detail() {
        const TOKEN: &str = "upstream-sensitive-token";
        let body = format!(r#"{{"error":"failure for {TOKEN}"}}"#);
        let server =
            SequenceServer::spawn_scenarios([Scenario::new("GET", "/v1/notes", 503, &body)]);
        let client = server.client_without_retry(TOKEN);

        let message = client
            .request_json::<Value>(Method::GET, &["notes"], NO_BODY)
            .await
            .expect_err("503 should retain safe error detail")
            .to_string();
        assert!(message.contains(r#"{"error":"failure for [REDACTED]"}"#));
        assert!(!message.contains(TOKEN));
        server.finish();
    }

    #[tokio::test]
    async fn bounds_generic_errors_and_redacts_the_token() {
        const TOKEN: &str = "fixture-sensitive-token";
        let body = format!("{} {TOKEN} {}", "x".repeat(280), "x".repeat(400));
        let server =
            SequenceServer::spawn_scenarios([Scenario::new("GET", "/v1/notes", 400, &body)]);
        let client = server.client_with_token(TOKEN);

        let message = client
            .request_json::<Value>(Method::GET, &["notes"], NO_BODY)
            .await
            .expect_err("400 should map to a generic API error")
            .to_string();
        assert!(!message.contains(TOKEN));
        assert!(message.contains("[REDACTED]"));
        assert!(message.ends_with('…'));
        assert!(message.chars().count() < 400);
        server.finish();
    }

    #[tokio::test]
    async fn rejects_invalid_json_without_exposing_the_body() {
        let server = SequenceServer::spawn_scenarios([Scenario::new(
            "GET",
            "/v1/me",
            200,
            "not-json-sensitive-content",
        )]);
        let client = server.client();

        let message = client
            .request_json::<Value>(Method::GET, &["me"], NO_BODY)
            .await
            .expect_err("invalid JSON should fail")
            .to_string();
        assert_eq!(
            message,
            "GET /v1/me: HackMD returned invalid JSON in a 200 OK response"
        );
        server.finish();
    }

    #[tokio::test]
    async fn maps_network_and_timeout_failures() {
        let disconnect = SequenceServer::spawn_disconnect();
        let network_client = disconnect.client_without_retry("fixture-token");
        assert!(matches!(
            network_client
                .request_json::<Value>(Method::GET, &["me"], NO_BODY)
                .await,
            Err(HackmdError::Network { .. })
        ));
        assert_eq!(disconnect.finish().len(), 1);

        let server = SequenceServer::spawn_scenarios([Scenario::new(
            "GET",
            "/v1/me",
            200,
            r#"{"ok":true}"#,
        )
        .delay(Duration::from_millis(100))]);
        let timeout_config = Config::for_loopback_test_with_timeout(
            &server.api_url,
            "fixture-token",
            Duration::from_millis(20),
        );
        let timeout_client =
            HackmdClient::new(timeout_config).expect("timeout fixture client should build");
        assert!(matches!(
            timeout_client
                .request_json::<Value>(Method::GET, &["me"], NO_BODY)
                .await,
            Err(HackmdError::Timeout { .. })
        ));
        server.finish();
    }

    #[tokio::test]
    async fn an_unknown_team_is_refused_after_one_teams_request() {
        let fixture =
            crate::fixture::SequenceServer::spawn_scenarios([crate::fixture::Scenario::new(
                "GET",
                "/v1/teams",
                200,
                r#"[{"id":"t","name":"Core","path":"core"}]"#,
            )]);
        let error = fixture
            .client()
            .ensure_team_exists(&crate::models::Workspace::Team {
                team_path: "elsewhere".to_owned(),
            })
            .await
            .expect_err("a team the account lacks should be refused");
        assert!(matches!(error, HackmdError::UnknownTeam { .. }));
        assert_eq!(fixture.finish().len(), 1);
    }

    #[tokio::test]
    async fn missing_token_fails_before_network_io() {
        let client = HackmdClient::new(Config::for_tests()).expect("test client should build");
        assert!(matches!(
            client
                .request_json::<Value>(Method::GET, &["me"], NO_BODY)
                .await,
            Err(HackmdError::MissingToken { .. })
        ));
    }

    #[tokio::test]
    async fn dot_segments_fail_before_they_can_change_the_route() {
        let client = HackmdClient::new(Config::for_tests()).expect("test client should build");
        for segments in [&["notes", "."][..], &["teams", "..", "notes", "id"][..]] {
            let error = client
                .request_json::<Value>(Method::DELETE, segments, NO_BODY)
                .await
                .expect_err("dot segments must be refused");
            assert_eq!(
                error.to_string(),
                "HackMD API path segments such as note IDs and team paths must not be . or .."
            );
        }
        // Dots inside an ID are ordinary characters.
        assert!(client.url_for_segments(&["notes", "a.b", "..c"]).is_ok());
    }

    #[tokio::test]
    async fn a_body_over_the_cap_is_refused_before_it_is_read() {
        let server = SequenceServer::spawn_scenarios([
            Scenario::new("GET", "/v1/big", 200, "0123456789"),
            Scenario::new("GET", "/v1/big", 200, "0123456789"),
        ]);
        let url = format!("{}/big", server.api_url);
        let response = reqwest::get(&url).await.expect("fixture should answer");
        assert!(matches!(
            super::read_body_capped(response, 4).await,
            Err(super::BodyError::TooLarge)
        ));
        let response = reqwest::get(&url).await.expect("fixture should answer");
        assert!(matches!(
            super::read_body_capped(response, 10).await,
            Ok(body) if body == b"0123456789"
        ));
        server.finish();
    }

    #[tokio::test]
    async fn a_rate_limit_reset_beyond_the_backoff_cap_is_not_retried() {
        let server = SequenceServer::spawn_scenarios([Scenario::new(
            "GET",
            "/v1/notes",
            429,
            r#"{"error":"slow down"}"#,
        )
        .response_header("retry-after", "60")]);
        let error = server
            .client()
            .request_json::<Value>(Method::GET, &["notes"], NO_BODY)
            .await
            .expect_err("a long reset should come straight back");
        assert!(matches!(error, HackmdError::RateLimited { .. }));
        assert_eq!(server.finish().len(), 1, "no retry inside the reset window");
    }

    #[tokio::test]
    async fn blank_resource_identifier_fails_before_token_or_network() {
        let client = HackmdClient::new(Config::for_tests()).expect("test client should build");
        assert!(matches!(
            client
                .request_json::<Value>(Method::GET, &["notes", "  "], NO_BODY)
                .await,
            Err(HackmdError::EmptyPathSegment)
        ));
    }

    #[tokio::test]
    async fn typed_profile_and_team_operations_use_discovery_routes() {
        let profile_server = SequenceServer::spawn_scenarios([Scenario::new(
            "GET",
            "/v1/me",
            200,
            r#"{"id":"user-id","name":"Alice","email":"alice@example.test","userPath":"alice","photo":null,"teams":[]}"#,
        )]);
        let profile_client = profile_server.client();
        let profile = profile_client
            .get_me()
            .await
            .expect("profile should deserialize");
        assert_eq!(profile.user_path, "alice");
        profile_server.finish();

        let teams_server = SequenceServer::spawn_scenarios([Scenario::new(
            "GET",
            "/v1/teams",
            200,
            r#"[{"id":"team-id","name":"Engineering","path":"engineering","description":null,"hardLimit":100,"visibility":"private"}]"#,
        )]);
        let teams_client = teams_server.client();
        let teams = teams_client
            .list_teams(false)
            .await
            .expect("teams should deserialize");
        assert_eq!(teams[0].path, "engineering");
        teams_server.finish();
    }

    #[tokio::test]
    async fn retries_only_gets_and_explicitly_idempotent_patches() {
        let get_server = SequenceServer::spawn_scenarios([
            Scenario::new("GET", "/v1/retry", 500, r#"{"error":"transient"}"#),
            Scenario::new("GET", "/v1/retry", 429, r#"{"error":"rate"}"#)
                .response_header("Retry-After", "0"),
            Scenario::new("GET", "/v1/retry", 200, r#"{"ok":true}"#),
        ]);
        let retry = crate::config::RetryConfig {
            max_retries: 3,
            initial_backoff: Duration::from_millis(1),
            max_backoff: Duration::from_millis(2),
        };
        let get_client = HackmdClient::new(Config::for_loopback_test_with_retry(
            &get_server.api_url,
            "fixture-token",
            retry,
        ))
        .expect("retry client should build");
        let response = get_client
            .request_json::<Value>(Method::GET, &["retry"], NO_BODY)
            .await
            .expect("GET should recover")
            .expect("GET should return JSON");
        assert_eq!(response, json!({"ok": true}));
        get_server.finish();

        let patch_server = SequenceServer::spawn_scenarios([
            Scenario::new("PATCH", "/v1/notes/id", 500, r#"{"error":"transient"}"#)
                .expect_header("content-type", "application/json")
                .expect_body("the replacement title", |body| {
                    body == r#"{"title":"same replacement"}"#
                }),
            Scenario::new("PATCH", "/v1/notes/id", 202, "")
                .expect_header("content-type", "application/json")
                .expect_body("the same replacement title", |body| {
                    body == r#"{"title":"same replacement"}"#
                }),
        ]);
        let patch_client = HackmdClient::new(Config::for_loopback_test_with_retry(
            &patch_server.api_url,
            "fixture-token",
            retry,
        ))
        .expect("retry client should build");
        patch_client
            .request_json_idempotent::<Value>(
                Method::PATCH,
                &["notes", "id"],
                Some(&json!({"title": "same replacement"})),
            )
            .await
            .expect("idempotent PATCH should recover");
        patch_server.finish();

        let post_server = SequenceServer::spawn_scenarios([Scenario::new(
            "POST",
            "/v1/notes",
            500,
            r#"{"error":"do not retry"}"#,
        )
        .expect_header("content-type", "application/json")
        .expect_body("an empty JSON object", |body| body == "{}")]);
        let post_client = HackmdClient::new(Config::for_loopback_test_with_retry(
            &post_server.api_url,
            "fixture-token",
            retry,
        ))
        .expect("retry client should build");
        assert!(matches!(
            post_client
                .request_json::<Value>(Method::POST, &["notes"], Some(&json!({})))
                .await,
            Err(HackmdError::Upstream { .. })
        ));
        post_server.finish();
    }
}
