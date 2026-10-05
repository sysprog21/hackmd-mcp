use reqwest::{Method, StatusCode};
use serde::{Serialize, de::DeserializeOwned};
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
use error::{RateLimitHeaders, map_status_error, parse_header, request_error, transport_error};
pub(crate) use readback::Readback;
use readback::{poll_readback_sized, transfer_allowance};

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
        // Redirects are refused: the API never sends one, and following a 307
        // would replay a note body to whatever origin it named. Timeouts are
        // per request, in `send`: reqwest's own read timeout is armed once per
        // request, so it would cut a long upload short.
        let http = reqwest::Client::builder()
            .connect_timeout(config.connect_timeout())
            .redirect(reqwest::redirect::Policy::none())
            .https_only(config.api_url().scheme() == "https")
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

    pub(crate) async fn restore_note(&self, note_id: &str) -> Result<(), HackmdError> {
        self.write_idempotent(Method::PUT, &["trash", note_id, "restore"], NO_BODY)
            .await
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
                    if let Some(outcome) = flight.wait().await {
                        return outcome;
                    }
                    bypass_cache = false;
                }
                CacheLookup::Fill { generation, flight } => {
                    let fill = CacheFill::new(&self.notes, workspace.clone(), generation, flight);
                    let outcome = self
                        .get_required::<Vec<NoteResponse>>(&workspace_route(workspace, &["notes"]))
                        .await
                        .map(Arc::from);

                    // A list a write overtook is fetched again: it may lack the
                    // note just written.
                    if fill.complete(&outcome) || outcome.is_err() {
                        return outcome;
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

    /// The one note PATCH. Sent once through `write`: every payload carries a
    /// body, and nearly every body was read or checked just before.
    pub(crate) async fn update_note(
        &self,
        workspace: &Workspace,
        note_id: &str,
        payload: &UpdateNoteRequest<'_>,
    ) -> Result<(), HackmdError> {
        let segments = workspace_route(workspace, &["notes", note_id]);
        self.write(Method::PATCH, &segments, Some(payload)).await
    }

    /// A note with its Markdown body taken out, for the tools that work on
    /// the body: a note that comes back without one is an error, not an
    /// empty body.
    pub(crate) async fn get_note_body(
        &self,
        workspace: &Workspace,
        note_id: &str,
    ) -> Result<(NoteResponse, String), HackmdError> {
        let mut note = self.get_note(workspace, note_id).await?;
        let body = note
            .content
            .take()
            .ok_or_else(|| HackmdError::MissingContent {
                note_id: note_id.to_owned(),
            })?;
        Ok((note, body))
    }

    /// Replaces a note's body and returns the first read that shows it. A
    /// body that never shows up within the read-back window is an error, not
    /// a success the caller would record.
    pub(crate) async fn write_note_body(
        &self,
        workspace: &Workspace,
        note_id: &str,
        body: &str,
    ) -> Result<NoteResponse, HackmdError> {
        self.update_note(workspace, note_id, &UpdateNoteRequest::new(body))
            .await?;
        self.confirm_note_write(workspace, note_id, body.len(), |note| {
            note.content.as_deref() == Some(body)
        })
        .await
    }

    /// Reads a just-written note until `accepted` holds, returning that read.
    /// A write that never shows up within the read-back window is
    /// `ReadbackMismatch`, not a success.
    pub(crate) async fn confirm_note_write(
        &self,
        workspace: &Workspace,
        note_id: &str,
        written_bytes: usize,
        accepted: impl Fn(&NoteResponse) -> bool,
    ) -> Result<NoteResponse, HackmdError> {
        self.poll_readback(
            written_bytes,
            || self.get_note(workspace, note_id),
            accepted,
        )
        .await?
        .confirmed_or(|| HackmdError::ReadbackMismatch {
            note_id: note_id.to_owned(),
        })
    }

    pub(crate) async fn delete_note(
        &self,
        workspace: &Workspace,
        note_id: &str,
    ) -> Result<(), HackmdError> {
        let segments = workspace_route(workspace, &["notes", note_id]);
        self.write(Method::DELETE, &segments, NO_BODY).await
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
    ) -> Result<(), HackmdError> {
        let segments = workspace_route(workspace, &["folders", folder_id]);
        self.write_idempotent(Method::PATCH, &segments, Some(payload))
            .await
    }

    pub(crate) async fn delete_folder(
        &self,
        workspace: &Workspace,
        folder_id: &str,
    ) -> Result<(), HackmdError> {
        let segments = workspace_route(workspace, &["folders", folder_id]);
        self.write(Method::DELETE, &segments, NO_BODY).await
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
    ) -> Result<(), HackmdError> {
        let segments = workspace_route(workspace, &["folders", "folder-order"]);
        self.write(Method::PUT, &segments, Some(&json!({"order": order})))
            .await
    }

    pub(crate) async fn upload_note_image(
        &self,
        note_id: &str,
        file_name: &str,
        mime: &str,
        image: tokio::fs::File,
        size_bytes: u64,
    ) -> Result<ImageUploadResponse, HackmdError> {
        let (url, path, token) = self.authorized("POST", &["notes", note_id, "images"])?;
        let form = reqwest::multipart::Form::new().part(
            "image",
            reqwest::multipart::Part::stream_with_length(image, size_bytes)
                .file_name(file_name.to_owned())
                .mime_str(mime)
                .map_err(|_| HackmdError::InvalidPayload)?,
        );
        tracing::debug!(method = "POST", path = %path, "HackMD request started");
        let size = usize::try_from(size_bytes).unwrap_or(usize::MAX);
        let request = self
            .http
            .post(url)
            .timeout(self.ceiling(size))
            .bearer_auth(token)
            .multipart(form);
        let response = self
            .until_headers(request, size)
            .await
            .map_err(|error| error.into_hackmd("POST", &path))?;
        let status = response.status();
        let rate_limit = RateLimitHeaders::from_headers(response.headers());
        let bytes = self
            .read_body(response)
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
        let reply = Reply::check(
            status,
            "POST".to_owned(),
            path.clone(),
            bytes,
            token,
            rate_limit,
        )?;

        // Unlike other writes, an unreadable reply here stays `upstream`, retry
        // later: no tool can look for an uploaded image, and uploading it again
        // only leaves an unused copy.
        match reply.json() {
            Ok(Some(uploaded)) => Ok(uploaded),
            Ok(None) => Err(HackmdError::EmptyResponse {
                method: "POST".to_owned(),
                path,
            }),
            Err(_) => Err(HackmdError::InvalidJson {
                method: "POST".to_owned(),
                path,
                status,
            }),
        }
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
            None => Err(HackmdError::unreadable_reply(
                method.as_str().to_owned(),
                self.url_for_segments(segments)?.path().to_owned(),
                None,
            )),
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
        self.send(method, path_segments, body, retryable, true)
            .await?
            .json()
    }

    /// Sends a write whose reply nobody reads, without retrying it: success
    /// is the status alone. The body is never parsed, because `HackMD`
    /// answers some writes with `{}` or plain text, and a write that landed
    /// must not come back as an error the agent would answer by repeating it.
    async fn write(
        &self,
        method: Method,
        path_segments: &[&str],
        body: Option<&(impl Serialize + Sync)>,
    ) -> Result<(), HackmdError> {
        self.send(method, path_segments, body, false, false)
            .await
            .map(drop)
    }

    /// `write`, retried because repeating it lands on the same state:
    /// `HackMD`'s PATCH and PUT routes set fields to given values rather than
    /// accumulating. Only for a payload of the caller's own values: one built
    /// from a read (a note body, the folder-order map) goes through `write`,
    /// because a retry after backoff would send it back over edits made while
    /// waiting.
    async fn write_idempotent(
        &self,
        method: Method,
        path_segments: &[&str],
        body: Option<&(impl Serialize + Sync)>,
    ) -> Result<(), HackmdError> {
        self.send(method, path_segments, body, true, false)
            .await
            .map(drop)
    }

    /// Drops every cached note list around a request that is not a read:
    /// once now and once more when the returned guard is dropped, which is
    /// every way out of the request, error paths included. The first covers
    /// a write that fails with a timeout yet still landed on `HackMD`; the
    /// second covers a concurrent list that refilled the cache while the
    /// write was in flight. Both live here rather than at each write site,
    /// where the next endpoint added would be free to forget.
    fn invalidate_list_cache_on_write(
        &self,
        method: &Method,
        path_segments: &[&str],
    ) -> Option<InvalidateOnDrop<'_>> {
        (method != Method::GET && changes_note_lists(method, path_segments)).then(|| {
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
        let readback = poll_readback_sized(
            written_bytes,
            self.config.request_timeout(),
            fetch,
            accepted,
        )
        .await?;
        if readback.confirmed {
            self.notes.invalidate();
        }
        Ok(readback)
    }

    async fn send(
        &self,
        method: Method,
        path_segments: &[&str],
        body: Option<&(impl Serialize + Sync)>,
        retryable: bool,
        read_success_body: bool,
    ) -> Result<Reply, HackmdError> {
        let method_text = method.as_str().to_owned();
        let (url, path, token) = self.authorized(&method_text, path_segments)?;
        let retry = self.config.retry();
        let mut retries = 0_u8;

        // Encoded once, not once per attempt: a retried request clones these
        // bytes rather than serializing the payload again.
        let body = body
            .map(|body| serde_json::to_vec(body))
            .transpose()
            .map_err(|_| HackmdError::InvalidPayload)?;

        // Repeating a large write after a transport failure re-uploads the
        // whole body for an outcome the caller has to check anyway.
        let retry_transport = retryable
            && body
                .as_ref()
                .is_none_or(|body| body.len() <= RETRY_BODY_MAX_BYTES);
        let body_len = body.as_ref().map_or(0, Vec::len);
        let mut request = self
            .http
            .request(method.clone(), url)
            .timeout(self.ceiling(body_len))
            .bearer_auth(token);
        if let Some(body) = body {
            request = request
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(body);
        }

        let _invalidate = self.invalidate_list_cache_on_write(&method, path_segments);
        tracing::debug!(method = %method_text, path = %path, "HackMD request started");
        let (status, bytes, rate_limit) = loop {
            let request = request.try_clone().ok_or(HackmdError::InvalidPayload)?;
            let response = match self.until_headers(request, body_len).await {
                Ok(response) => response,
                Err(_) if retry_transport && retries < retry.max_retries => {
                    sleep_before_retry(retries, None, retry, false).await;
                    retries += 1;
                    continue;
                }
                Err(error) => return Err(error.into_hackmd(&method_text, &path)),
            };
            let status = response.status();
            let retry_after = retry_after(response.headers());
            let rate_limit = RateLimitHeaders::from_headers(response.headers());

            // A write whose reply nobody reads is done once its status says so.
            // Reading on would let a body that breaks off turn a write that
            // landed into a retry, or into an error.
            if status.is_success() && !read_success_body {
                break (status, Vec::new(), rate_limit);
            }
            let bytes = match self.read_body(response).await {
                Ok(bytes) => bytes,
                Err(BodyError::Transport(_) | BodyError::Stalled)
                    if retry_transport && retries < retry.max_retries =>
                {
                    sleep_before_retry(retries, None, retry, false).await;
                    retries += 1;
                    continue;
                }

                // The status already says what happened; a body that broke off
                // loses only its detail, not the verdict.
                Err(_) if !status.is_success() => Vec::new(),
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

        Reply::check(status, method_text, path, bytes, token, rate_limit)
    }

    /// Sends `request` and waits for its response headers: the request
    /// timeout, plus the upload of `body_len` bytes at the slowest rate still
    /// accepted, since headers come only after the body is sent.
    async fn until_headers(
        &self,
        request: reqwest::RequestBuilder,
        body_len: usize,
    ) -> Result<reqwest::Response, BodyError> {
        let budget = self.config.request_timeout() + transfer_allowance(body_len);
        match tokio::time::timeout(budget, request.send()).await {
            Ok(result) => result.map_err(BodyError::Transport),
            Err(_) => Err(BodyError::Stalled),
        }
    }

    /// Reads a response body, refusing it past the response cap and giving
    /// up when no data arrives for the request timeout: a stall ends a read,
    /// a slow but steady transfer does not.
    async fn read_body(&self, response: reqwest::Response) -> Result<Vec<u8>, BodyError> {
        read_body_capped(response, RESPONSE_MAX_BYTES, self.config.request_timeout()).await
    }

    /// The outer bound on a whole request carrying `body_len` bytes, should
    /// a server drip data just fast enough to never stall: both transfers at
    /// the slowest rate still accepted.
    fn ceiling(&self, body_len: usize) -> Duration {
        self.config.request_timeout()
            + transfer_allowance(body_len.saturating_add(RESPONSE_MAX_BYTES))
    }

    /// What every request starts from: its URL, the path its logs and errors
    /// name, and the token, whose absence fails before anything is sent.
    fn authorized(
        &self,
        method: &str,
        path_segments: &[&str],
    ) -> Result<(Url, String, &str), HackmdError> {
        let url = self.url_for_segments(path_segments)?;
        let path = url.path().to_owned();
        let token = self
            .config
            .api_token()
            .ok_or_else(|| HackmdError::MissingToken {
                method: method.to_owned(),
                path: path.clone(),
            })?;
        Ok((url, path, token))
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

        // The URL library also drops tabs and newlines inside a segment before
        // judging dots, so `.\t.` would become `..` after the check above.
        if path_segments
            .iter()
            .any(|segment| segment.chars().any(char::is_control))
        {
            return Err(HackmdError::ControlInPathSegment);
        }
        let mut url = self.config.api_url().clone();
        let mut segments = url
            .path_segments_mut()
            .map_err(|()| HackmdError::InvalidBaseUrl)?;
        segments.pop_if_empty();
        segments.extend(path_segments);
        drop(segments);

        // Whatever else the library might normalize, one segment in must be one
        // segment out, or the request names another route.
        let count = |url: &Url| {
            url.path_segments().map_or(0, |segments| {
                segments.filter(|segment| !segment.is_empty()).count()
            })
        };
        if count(&url) != count(self.config.api_url()) + path_segments.len() {
            return Err(HackmdError::DotPathSegment);
        }
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
    parse_header(headers, "x-ratelimit-userreset").map(Duration::from_secs)
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

/// The largest request body retried after a transport failure. Today's
/// retried writes (folder PATCH, trash restore) are far below it; it keeps a
/// future large idempotent write from re-uploading its whole body blind.
const RETRY_BODY_MAX_BYTES: usize = 1024 * 1024;

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
    /// No response, or no further data, within the request timeout.
    Stalled,
    TooLarge,
}

impl BodyError {
    fn into_hackmd(self, method: &str, path: &str) -> HackmdError {
        match self {
            Self::Transport(error) => request_error(&error, method.to_owned(), path.to_owned()),
            Self::Stalled => transport_error(true, false, method.to_owned(), path.to_owned()),
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
    stall: Duration,
) -> Result<Vec<u8>, BodyError> {
    let declared = response
        .content_length()
        .map(|length| usize::try_from(length).unwrap_or(usize::MAX));
    if declared.is_some_and(|length| length > cap) {
        return Err(BodyError::TooLarge);
    }
    let mut body = Vec::with_capacity(declared.unwrap_or(0));
    loop {
        let next = tokio::time::timeout(stall, response.chunk())
            .await
            .map_err(|_| BodyError::Stalled)?;
        let Some(chunk) = next.map_err(BodyError::Transport)? else {
            break;
        };
        if body.len() + chunk.len() > cap {
            return Err(BodyError::TooLarge);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// A response whose status was a success, its body not yet decoded.
struct Reply {
    method: String,
    path: String,
    status: StatusCode,
    bytes: Vec<u8>,
}

impl Reply {
    /// Turns a failure status into the error named by it, and anything else
    /// into a `Reply`.
    fn check(
        status: StatusCode,
        method: String,
        path: String,
        bytes: Vec<u8>,
        token: &str,
        rate_limit: RateLimitHeaders,
    ) -> Result<Self, HackmdError> {
        if !status.is_success() {
            return Err(map_status_error(
                status, method, path, &bytes, token, rate_limit,
            ));
        }
        Ok(Self {
            method,
            path,
            status,
            bytes,
        })
    }

    /// `None` for `HackMD`'s empty success bodies, otherwise the decoded JSON.
    fn json<T: DeserializeOwned>(self) -> Result<Option<T>, HackmdError> {
        if self.bytes.is_empty() {
            return Ok(None);
        }
        serde_json::from_slice(&self.bytes)
            .map(Some)
            .map_err(|_| HackmdError::unreadable_reply(self.method, self.path, Some(self.status)))
    }
}

/// Names the absent body at the many call sites that have none: `None` alone
/// cannot infer the payload type once the parameter is generic.
const NO_BODY: Option<&Value> = None;

fn includes_team(teams: &[TeamResponse], team_path: &str) -> bool {
    teams.iter().any(|team| team.path == team_path)
}

/// Whether a write to this route can change a note list. A folder rename or
/// reorder cannot: a listed note carries no folder fields. A folder DELETE
/// still counts, since what it does to the notes inside is not verified.
fn changes_note_lists(method: &Method, path_segments: &[&str]) -> bool {
    if method == Method::DELETE {
        return true;
    }
    let resource = if let ["teams", _, rest @ ..] = path_segments {
        rest
    } else {
        path_segments
    };
    resource.first() != Some(&"folders")
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
                .expect_body("the renamed title", |body| {
                    body == r#"{"title":"Renamed","content":"body"}"#
                }),
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
                    ..UpdateNoteRequest::new("body".to_owned())
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

    /// A failed fill is shared too: waiters must not each repeat a request
    /// that just hit a rate limit or an outage.
    #[tokio::test]
    async fn concurrent_callers_share_a_failed_list_request() {
        let server = SequenceServer::spawn_scenarios([Scenario::new(
            "GET",
            "/v1/notes",
            503,
            r#"{"error":"down"}"#,
        )
        .delay(Duration::from_millis(100))]);
        let client = server.client_without_retry("fixture-token");

        let (first, second) = tokio::join!(
            client.list_notes(&Workspace::Personal, false),
            client.list_notes(&Workspace::Personal, false)
        );

        assert!(matches!(first, Err(HackmdError::Upstream { .. })));
        assert!(matches!(second, Err(HackmdError::Upstream { .. })));
        assert_eq!(server.finish().len(), 1);
    }

    /// A note list carries no folder fields, so a folder write leaves it.
    #[tokio::test]
    async fn a_folder_write_keeps_cached_note_lists() {
        let server = SequenceServer::spawn_scenarios([
            Scenario::new("GET", "/v1/notes", 200, NOTES),
            Scenario::new("PUT", "/v1/folders/folder-order", 200, ""),
        ]);
        let client = server.client_with_cache();
        client
            .list_notes(&Workspace::Personal, false)
            .await
            .expect("first list should fetch");
        client
            .set_folder_order(&Workspace::Personal, &std::collections::BTreeMap::new())
            .await
            .expect("folder order should save");
        client
            .list_notes(&Workspace::Personal, false)
            .await
            .expect("second list should come from cache");
        assert_eq!(server.finish().len(), 2);
    }

    /// What a folder DELETE does to the notes inside is unverified, so it
    /// still clears cached note lists.
    #[tokio::test]
    async fn a_folder_delete_clears_cached_note_lists() {
        let server = SequenceServer::spawn_scenarios([
            Scenario::new("GET", "/v1/notes", 200, NOTES),
            Scenario::new("DELETE", "/v1/folders/f", 204, ""),
            Scenario::new("GET", "/v1/notes", 200, NOTES),
        ]);
        let client = server.client_with_cache();
        client
            .list_notes(&Workspace::Personal, false)
            .await
            .expect("first list should fetch");
        client
            .delete_folder(&Workspace::Personal, "f")
            .await
            .expect("folder delete should succeed");
        client
            .list_notes(&Workspace::Personal, false)
            .await
            .expect("list after the delete should fetch again");
        assert_eq!(server.finish().len(), 3);
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
            &crate::fixture::scratch_files(),
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
        assert_eq!(page.meta().total, 10_000);
        assert_eq!(page.meta().count, 100);

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
                    ..UpdateNoteRequest::new("body".to_owned())
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

    /// A write that landed is a success whatever its body says: `HackMD`
    /// answers PATCH with `202 {}`, and an error here would invite a retry.
    #[tokio::test]
    async fn a_write_succeeds_on_status_alone() {
        let server = SequenceServer::spawn_scenarios([
            Scenario::new("PATCH", "/v1/notes/id", 202, "{}"),
            Scenario::new("PATCH", "/v1/teams/team/folders/folder", 200, "OK"),
        ]);
        let client = server.client();
        client
            .update_note(
                &Workspace::Personal,
                "id",
                &UpdateNoteRequest {
                    title: Some("Renamed".to_owned()),
                    ..UpdateNoteRequest::new("body".to_owned())
                },
            )
            .await
            .expect("a 202 with an empty object should succeed");
        client
            .update_folder(
                &Workspace::Team {
                    team_path: "team".to_owned(),
                },
                "folder",
                &crate::dto::UpdateFolderRequest::default(),
            )
            .await
            .expect("a 200 with a non-JSON body should succeed");
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
            body == r#"{"content":"body","parentFolderId":null}"#
        })]);
        let update_client = update_server.client();
        update_client
            .update_note(
                &Workspace::Personal,
                "note/id",
                &UpdateNoteRequest {
                    parent_folder_id: Some(None),
                    ..UpdateNoteRequest::new("body".to_owned())
                },
            )
            .await
            .expect("personal note update should be accepted");
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

    /// A write that timed out may have landed: the caller is told to look,
    /// never to retry, since a second POST would create a second note.
    #[tokio::test]
    async fn a_write_that_timed_out_is_unconfirmed_not_retryable() {
        let server = SequenceServer::spawn_scenarios([Scenario::new(
            "POST",
            "/v1/notes",
            201,
            r#"{"id":"late"}"#,
        )
        .delay(Duration::from_millis(100))]);
        let client = HackmdClient::new(Config::for_loopback_test_with_timeout(
            &server.api_url,
            "fixture-token",
            Duration::from_millis(20),
        ))
        .expect("timeout fixture client should build");
        let error = client
            .request_json::<Value>(Method::POST, &["notes"], Some(&json!({})))
            .await
            .expect_err("a timed-out POST should fail");
        assert_eq!(
            error.to_string(),
            "POST /v1/notes: the write timed out; it may still have landed, so check whether it did before sending it again"
        );
        assert_eq!(
            crate::reply::ToolError::kind(&error),
            crate::reply::ErrorKind::Readback
        );
        server.finish();
    }

    /// A write that never connected never reached `HackMD`: that one is safe
    /// to retry, and is reported as a network failure, not as unconfirmed.
    #[tokio::test]
    async fn a_write_that_never_connected_is_a_network_error() {
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .expect("port should bind")
            .local_addr()
            .expect("address should resolve")
            .port();
        let client = HackmdClient::new(Config::for_loopback_test_no_retry(
            &format!("http://127.0.0.1:{port}/v1"),
            "fixture-token",
        ))
        .expect("client should build");
        assert!(matches!(
            client
                .request_json::<Value>(Method::POST, &["notes"], Some(&json!({})))
                .await,
            Err(HackmdError::Network { .. })
        ));
    }

    /// The URL library drops tabs and newlines inside a segment before it
    /// resolves dots, so these would otherwise reach another route.
    #[test]
    fn control_characters_never_reach_a_path() {
        let client = HackmdClient::new(Config::for_loopback_test(
            "http://127.0.0.1:9/v1",
            Some("fixture-token"),
        ))
        .expect("client should build");
        for segments in [
            &["teams", ".\t.", "notes", "ID"][..],
            &["teams", "T", "notes", ".\r."],
            &["folders", "\t.."],
            &["notes", "a\nb"],
        ] {
            assert!(
                matches!(
                    client.url_for_segments(segments),
                    Err(HackmdError::ControlInPathSegment)
                ),
                "{segments:?}"
            );
        }
        assert_eq!(
            client
                .url_for_segments(&["teams", "t", "notes", "a/b"])
                .expect("an encoded slash is one segment")
                .path(),
            "/v1/teams/t/notes/a%2Fb"
        );
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
            super::read_body_capped(response, 4, Duration::from_secs(5)).await,
            Err(super::BodyError::TooLarge)
        ));
        let response = reqwest::get(&url).await.expect("fixture should answer");
        assert!(matches!(
            super::read_body_capped(response, 10, Duration::from_secs(5)).await,
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
        let response = get_server
            .client_with_fast_retry()
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
        patch_server
            .client_with_fast_retry()
            .write_idempotent(
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
        assert!(matches!(
            post_server
                .client_with_fast_retry()
                .request_json::<Value>(Method::POST, &["notes"], Some(&json!({})))
                .await,
            Err(HackmdError::WriteUnconfirmed { .. })
        ));
        post_server.finish();

        // Every note PATCH carries a body that was read or checked just before,
        // so it is sent once: a retrying client would reach the 202.
        let note_server = SequenceServer::spawn_scenarios([
            Scenario::new("PATCH", "/v1/notes/id", 503, ""),
            Scenario::new("PATCH", "/v1/notes/id", 202, ""),
        ]);
        assert!(matches!(
            note_server
                .client_with_fast_retry()
                .update_note(&Workspace::Personal, "id", &UpdateNoteRequest::new("body"))
                .await,
            Err(HackmdError::WriteUnconfirmed { .. })
        ));

        // Not after a rate limit either; the error says the retry must start by
        // reading the note again.
        let limited_server = SequenceServer::spawn_scenarios([
            Scenario::new("PATCH", "/v1/notes/id", 429, "").response_header("Retry-After", "0"),
            Scenario::new("PATCH", "/v1/notes/id", 202, ""),
        ]);
        let error = limited_server
            .client_with_fast_retry()
            .update_note(&Workspace::Personal, "id", &UpdateNoteRequest::new("body"))
            .await
            .expect_err("a rate-limited note write is not retried");
        assert!(matches!(error, HackmdError::RateLimited { .. }), "{error}");
        assert!(error.to_string().contains("read the note again"), "{error}");
    }
}
