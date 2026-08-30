use reqwest::{Method, StatusCode};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, SystemTime};
use thiserror::Error;
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

/// HTTP client shared by all `HackMD` tool handlers.
#[derive(Debug)]
pub(crate) struct HackmdClient {
    config: Config,
    http: reqwest::Client,
    notes: NotesCache,
}

impl HackmdClient {
    pub(crate) fn new(config: Config) -> Result<Self, HackmdError> {
        let http = reqwest::Client::builder()
            .connect_timeout(config.connect_timeout())
            .timeout(config.request_timeout())
            .build()
            .map_err(|_| HackmdError::ClientBuild)?;
        let notes = NotesCache::new(config.list_cache_ttl());
        Ok(Self {
            config,
            http,
            notes,
        })
    }

    pub(crate) fn has_api_token(&self) -> bool {
        self.config.has_api_token()
    }

    pub(crate) async fn get_me(&self) -> Result<ProfileResponse, HackmdError> {
        self.get_required(&["me"]).await
    }

    pub(crate) async fn list_teams(&self) -> Result<Vec<TeamResponse>, HackmdError> {
        self.get_required(&["teams"]).await
    }

    pub(crate) async fn ensure_team_exists(
        &self,
        workspace: &Workspace,
    ) -> Result<(), HackmdError> {
        let Workspace::Team { team_path } = workspace else {
            return Ok(());
        };
        if self
            .list_teams()
            .await?
            .iter()
            .any(|team| team.path == *team_path)
        {
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

    pub(crate) async fn restore_note(&self, note_id: &str) -> Result<Option<Value>, HackmdError> {
        self.request_json_idempotent(Method::PUT, &["trash", note_id, "restore"], NO_BODY)
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

    pub(crate) async fn delete_note(
        &self,
        workspace: &Workspace,
        note_id: &str,
    ) -> Result<Option<Value>, HackmdError> {
        let segments = workspace_route(workspace, &["notes", note_id]);
        self.request_json(Method::DELETE, &segments, NO_BODY).await
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
        let bytes = response
            .bytes()
            .await
            .map_err(|error| request_error(&error, "POST".to_owned(), path.clone()))?;
        if status == StatusCode::PAYLOAD_TOO_LARGE {
            return Err(HackmdError::ImageTooLarge { path });
        }
        if !status.is_success() {
            return Err(map_status_error(
                status,
                "POST".to_owned(),
                path,
                &bytes,
                token,
                rate_limit,
            ));
        }
        serde_json::from_slice(&bytes).map_err(|_| HackmdError::InvalidJson {
            method: "POST".to_owned(),
            path,
            status,
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

    /// Drops every cached note list once a request that is not a read has been
    /// issued. Called before the request as well as after it: a write that
    /// fails with a timeout may still have landed on `HackMD`, and the error
    /// paths return without reaching the second call. The second call covers
    /// the opposite order, where a concurrent list refilled the cache while the
    /// write was in flight. Both live here rather than at each write site,
    /// where the next endpoint added would be free to forget.
    fn invalidate_list_cache_on_write(&self, method: &Method) {
        if method != Method::GET {
            self.notes.invalidate();
        }
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

        self.invalidate_list_cache_on_write(&method);
        tracing::debug!(method = %method_text, path = %path, "HackMD request started");
        let (status, bytes, rate_limit) = loop {
            let mut request = self
                .http
                .request(method.clone(), url.clone())
                .bearer_auth(token);
            if let Some(body) = body.as_ref() {
                request = request
                    .header(reqwest::header::CONTENT_TYPE, "application/json")
                    .body(body.clone());
            }
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
            let bytes = match response.bytes().await {
                Ok(bytes) => bytes,
                Err(_) if retryable && retries < retry.max_retries => {
                    sleep_before_retry(retries, None, retry, false).await;
                    retries += 1;
                    continue;
                }
                Err(error) => {
                    return Err(request_error(&error, method_text.clone(), path.clone()));
                }
            };
            if retryable
                && retries < retry.max_retries
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

        self.invalidate_list_cache_on_write(&method);

        if !status.is_success() {
            return Err(map_status_error(
                status,
                method_text,
                path,
                &bytes,
                token,
                rate_limit,
            ));
        }
        if bytes.is_empty() {
            return Ok(None);
        }

        serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|_| HackmdError::InvalidJson {
                method: method_text,
                path,
                status,
            })
    }

    fn url_for_segments(&self, path_segments: &[&str]) -> Result<Url, HackmdError> {
        if path_segments
            .iter()
            .any(|segment| segment.trim().is_empty())
        {
            return Err(HackmdError::EmptyPathSegment);
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
/// and falling back to `HackMD`'s own rate-limit reset. Note that
/// `sleep_before_retry` clamps the result: a reset further out than the maximum
/// backoff is not waited for, it is retried and allowed to fail.
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

/// Names the absent body at the many call sites that have none: `None` alone
/// cannot infer the payload type once the parameter is generic.
const NO_BODY: Option<&Value> = None;

const MAX_CACHED_WORKSPACES: usize = 32;

/// One workspace's list, when it was fetched, and its LRU position.
#[derive(Debug)]
struct CachedNotes {
    stored: tokio::time::Instant,
    last_access: u64,
    notes: Arc<[NoteResponse]>,
}

/// A short-lived copy of a workspace's note list.
///
/// `HackMD` has no note-list pagination and no conditional GET, so listing is
/// all-or-nothing and the same list backs both the list tool and every URL
/// reference resolution. A TTL of zero disables the cache, which is what the
/// tests use so their request counts stay meaningful.
#[derive(Debug)]
struct NotesCache {
    ttl: Duration,
    state: Mutex<NotesCacheState>,
}

#[derive(Debug, Default)]
struct NotesCacheState {
    generation: u64,
    access_clock: u64,
    entries: HashMap<Workspace, CachedNotes>,
    flights: HashMap<Workspace, Arc<CacheFlight>>,
}

#[derive(Debug)]
struct CacheFlight {
    done: AtomicBool,
    notify: tokio::sync::Notify,
}

impl CacheFlight {
    fn new() -> Self {
        Self {
            done: AtomicBool::new(false),
            notify: tokio::sync::Notify::new(),
        }
    }

    async fn wait(&self) {
        loop {
            let notified = self.notify.notified();
            if self.done.load(Ordering::Acquire) {
                return;
            }
            notified.await;
        }
    }

    fn finish(&self) {
        self.done.store(true, Ordering::Release);
        self.notify.notify_waiters();
    }
}

enum CacheLookup {
    Hit(Arc<[NoteResponse]>),
    Wait(Arc<CacheFlight>),
    Fill {
        generation: u64,
        flight: Arc<CacheFlight>,
    },
}

impl NotesCache {
    fn new(ttl: Duration) -> Self {
        Self {
            ttl,
            state: Mutex::new(NotesCacheState::default()),
        }
    }

    fn begin(&self, workspace: &Workspace, bypass_cache: bool) -> CacheLookup {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !bypass_cache && !self.ttl.is_zero() {
            state.access_clock = state.access_clock.wrapping_add(1);
            let access = state.access_clock;
            if let Some(cached) = state.entries.get_mut(workspace)
                && cached.stored.elapsed() < self.ttl
            {
                cached.last_access = access;
                tracing::debug!(cache_event = "hit", "HackMD note-list cache");
                return CacheLookup::Hit(Arc::clone(&cached.notes));
            }
            if state.entries.remove(workspace).is_some() {
                tracing::debug!(
                    cache_event = "eviction",
                    reason = "expired",
                    "HackMD note-list cache"
                );
            }
        }
        if let Some(flight) = state.flights.get(workspace) {
            tracing::debug!(
                cache_event = "miss",
                coalesced = true,
                "HackMD note-list cache"
            );
            return CacheLookup::Wait(Arc::clone(flight));
        }
        tracing::debug!(
            cache_event = "miss",
            coalesced = false,
            "HackMD note-list cache"
        );
        let generation = state.generation;
        let flight = Arc::new(CacheFlight::new());
        state.flights.insert(workspace.clone(), Arc::clone(&flight));
        CacheLookup::Fill { generation, flight }
    }

    fn finish(
        &self,
        workspace: &Workspace,
        generation: u64,
        flight: &Arc<CacheFlight>,
        notes: Option<&Arc<[NoteResponse]>>,
    ) -> bool {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut accepted = false;
        if state
            .flights
            .get(workspace)
            .is_some_and(|current| Arc::ptr_eq(current, flight))
        {
            state.flights.remove(workspace);
            if state.generation == generation
                && let Some(notes) = notes
            {
                accepted = true;
                if !self.ttl.is_zero() {
                    let now = tokio::time::Instant::now();
                    let expired = state
                        .entries
                        .iter()
                        .filter(|&(_key, cached)| cached.stored.elapsed() >= self.ttl)
                        .map(|(key, _cached)| key.clone())
                        .collect::<Vec<_>>();
                    for key in expired {
                        state.entries.remove(&key);
                        tracing::debug!(
                            cache_event = "eviction",
                            reason = "expired",
                            "HackMD note-list cache"
                        );
                    }
                    if !state.entries.contains_key(workspace)
                        && state.entries.len() >= MAX_CACHED_WORKSPACES
                        && let Some(lru) = state
                            .entries
                            .iter()
                            .min_by_key(|(_, cached)| cached.last_access)
                            .map(|(key, _)| key.clone())
                    {
                        state.entries.remove(&lru);
                        tracing::debug!(
                            cache_event = "eviction",
                            reason = "capacity",
                            "HackMD note-list cache"
                        );
                    }
                    state.access_clock = state.access_clock.wrapping_add(1);
                    let access = state.access_clock;
                    state.entries.insert(
                        workspace.clone(),
                        CachedNotes {
                            stored: now,
                            last_access: access,
                            notes: Arc::clone(notes),
                        },
                    );
                    tracing::debug!(cache_event = "fill", "HackMD note-list cache");
                }
            }
        }
        flight.finish();
        accepted
    }

    /// Called after any write. Clearing every workspace rather than one is
    /// deliberate: a note can move between workspaces, and the map holds at
    /// most a handful of entries.
    fn invalidate(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.generation = state.generation.wrapping_add(1);
        if !state.entries.is_empty() {
            tracing::debug!(
                cache_event = "eviction",
                reason = "invalidation",
                count = state.entries.len(),
                "HackMD note-list cache"
            );
        }
        state.entries.clear();
    }
}

struct CacheFill<'a> {
    cache: &'a NotesCache,
    workspace: Workspace,
    generation: u64,
    flight: Arc<CacheFlight>,
    completed: bool,
}

impl<'a> CacheFill<'a> {
    fn new(
        cache: &'a NotesCache,
        workspace: Workspace,
        generation: u64,
        flight: Arc<CacheFlight>,
    ) -> Self {
        Self {
            cache,
            workspace,
            generation,
            flight,
            completed: false,
        }
    }

    fn complete(mut self, notes: &Arc<[NoteResponse]>) -> bool {
        let accepted =
            self.cache
                .finish(&self.workspace, self.generation, &self.flight, Some(notes));
        self.completed = true;
        accepted
    }
}

impl Drop for CacheFill<'_> {
    fn drop(&mut self) {
        if !self.completed {
            let _ = self
                .cache
                .finish(&self.workspace, self.generation, &self.flight, None);
        }
    }
}

/// How long a write is given to become visible, and the first pause between
/// reads. The pause doubles so the window is covered in a handful of requests
/// rather than ten: `HackMD` allows 100 requests per five minutes, and a
/// foldered note creation spends several of them before ever polling.
const READBACK_WINDOW: Duration = Duration::from_secs(2);
const READBACK_FIRST_DELAY: Duration = Duration::from_millis(100);

/// What a read-back saw, and whether it satisfied the caller.
pub(crate) struct Readback<T> {
    pub(crate) value: T,
    pub(crate) confirmed: bool,
}

/// Re-reads a just-written resource until `accepted` holds.
///
/// `HackMD` applies some writes asynchronously, so the first read after a write
/// can still answer with the previous value. When the window expires the last
/// observation is still returned, with `confirmed: false`: a caller that treats
/// that as failure has its error, and one that wants to report the current
/// state has it without paying for another request.
///
pub(crate) async fn poll_readback<T, Fut>(
    fetch: impl FnMut() -> Fut,
    accepted: impl Fn(&T) -> bool,
) -> Result<Readback<T>, HackmdError>
where
    Fut: std::future::Future<Output = Result<T, HackmdError>>,
{
    poll_readback_with_policy(fetch, accepted, READBACK_WINDOW, READBACK_FIRST_DELAY).await
}

async fn poll_readback_with_policy<T, Fut>(
    mut fetch: impl FnMut() -> Fut,
    accepted: impl Fn(&T) -> bool,
    window: Duration,
    first_delay: Duration,
) -> Result<Readback<T>, HackmdError>
where
    Fut: std::future::Future<Output = Result<T, HackmdError>>,
{
    let started = tokio::time::Instant::now();
    let deadline = started + window;
    let mut delay = first_delay;
    let mut attempts = 0_u8;
    loop {
        attempts = attempts.saturating_add(1);
        let Ok(result) = tokio::time::timeout_at(deadline, fetch()).await else {
            crate::retry::record_readback(attempts, started.elapsed());
            return Err(HackmdError::ReadbackTimeout);
        };
        let value = match result {
            Ok(value) => value,
            Err(error) => {
                crate::retry::record_readback(attempts, started.elapsed());
                return Err(error);
            }
        };
        if accepted(&value) {
            crate::retry::record_readback(attempts, started.elapsed());
            return Ok(Readback {
                value,
                confirmed: true,
            });
        }
        if tokio::time::Instant::now() + delay >= deadline {
            crate::retry::record_readback(attempts, started.elapsed());
            return Ok(Readback {
                value,
                confirmed: false,
            });
        }
        tokio::time::sleep(delay).await;
        delay = delay.saturating_mul(2);
    }
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

fn request_error(error: &reqwest::Error, method: String, path: String) -> HackmdError {
    if error.is_timeout() {
        HackmdError::Timeout { method, path }
    } else {
        HackmdError::Network { method, path }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct RateLimitHeaders {
    user_limit: Option<u32>,
    user_remaining: Option<u32>,
    reset_after: Option<u64>,
}

impl RateLimitHeaders {
    fn from_headers(headers: &reqwest::header::HeaderMap) -> Self {
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

fn map_status_error(
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
    let text = String::from_utf8_lossy(body).replace(token, "[REDACTED]");
    let mut chars = text.chars();
    let mut bounded: String = chars.by_ref().take(MAX_CHARS).collect();
    if chars.next().is_some() {
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

impl From<HackmdError> for rmcp::model::CallToolResult {
    fn from(error: HackmdError) -> Self {
        crate::reply::error(error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use std::{sync::Arc, time::Duration};

    const NOTES: &str = r#"[{"id":"note-id","title":"Note"}]"#;

    use reqwest::Method;
    use serde_json::{Value, json};

    use super::{
        CacheFill, CacheLookup, HackmdClient, HackmdError, MAX_CACHED_WORKSPACES, NO_BODY,
        NotesCache, retry_after,
    };
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
        fixture::{FIXTURE_TOKEN, SequenceServer},
        models::Workspace,
    };

    #[tokio::test]
    async fn a_fresh_note_list_is_reused_until_a_write_invalidates_it() {
        // Two list responses for three list calls: the second call is served
        // from cache, and only the write in between forces another fetch.
        let server = SequenceServer::spawn([(200, NOTES), (202, ""), (200, NOTES)]);
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

        let requests = server.finish();
        assert_eq!(requests.len(), 3);
        assert!(requests[0].starts_with("GET /v1/notes HTTP/1.1\r\n"));
        assert!(requests[1].starts_with("PATCH /v1/notes/note-id HTTP/1.1\r\n"));
        assert!(requests[2].starts_with("GET /v1/notes HTTP/1.1\r\n"));
    }

    #[tokio::test]
    async fn refresh_bypasses_and_replaces_a_cached_note_list() {
        const RENAMED: &str = r#"[{"id":"note-id","title":"Renamed"}]"#;
        let server = SequenceServer::spawn([(200, NOTES), (200, RENAMED)]);
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
        assert_eq!(server.finish().len(), 2);
    }

    #[tokio::test]
    async fn concurrent_workspace_misses_share_one_list_request() {
        let server =
            crate::fixture::SequenceServer::spawn_delayed(200, NOTES, Duration::from_millis(100));
        let client = Arc::new(server.client_with_cache());

        let (first, second) = tokio::join!(
            client.list_notes(&Workspace::Personal, false),
            client.list_notes(&Workspace::Personal, false)
        );

        assert_eq!(first.expect("first list should succeed").len(), 1);
        assert_eq!(second.expect("second list should succeed").len(), 1);
        assert_eq!(server.finish().len(), 1);
    }

    #[test]
    fn invalidation_generation_rejects_an_older_in_flight_fill() {
        let cache = NotesCache::new(Duration::from_secs(60));
        let CacheLookup::Fill { generation, flight } = cache.begin(&Workspace::Personal, false)
        else {
            panic!("empty cache should start a fill");
        };
        cache.invalidate();
        let notes: Arc<[crate::dto::NoteResponse]> = Vec::new().into();

        assert!(!cache.finish(&Workspace::Personal, generation, &flight, Some(&notes)));
        let CacheLookup::Fill { generation, flight } = cache.begin(&Workspace::Personal, false)
        else {
            panic!("stale fill must not repopulate the cache");
        };
        let _ = cache.finish(&Workspace::Personal, generation, &flight, None);
    }

    #[tokio::test]
    async fn dropping_a_fill_leader_wakes_coalesced_waiters() {
        let cache = NotesCache::new(Duration::from_secs(60));
        let CacheLookup::Fill { generation, flight } = cache.begin(&Workspace::Personal, false)
        else {
            panic!("empty cache should start a fill");
        };
        let fill = CacheFill::new(&cache, Workspace::Personal, generation, Arc::clone(&flight));
        drop(fill);

        tokio::time::timeout(Duration::from_millis(50), flight.wait())
            .await
            .expect("abandoned fill should wake waiters");
    }

    #[test]
    fn cache_capacity_evicts_the_least_recently_used_workspace() {
        let cache = NotesCache::new(Duration::from_secs(60));
        let notes: Arc<[crate::dto::NoteResponse]> = Vec::new().into();
        for index in 0..MAX_CACHED_WORKSPACES {
            let workspace = Workspace::Team {
                team_path: format!("team-{index}"),
            };
            let CacheLookup::Fill { generation, flight } = cache.begin(&workspace, false) else {
                panic!("new workspace should miss");
            };
            assert!(cache.finish(&workspace, generation, &flight, Some(&notes)));
        }
        let recently_used = Workspace::Team {
            team_path: "team-0".to_owned(),
        };
        assert!(matches!(
            cache.begin(&recently_used, false),
            CacheLookup::Hit(_)
        ));
        let newest = Workspace::Team {
            team_path: format!("team-{MAX_CACHED_WORKSPACES}"),
        };
        let CacheLookup::Fill { generation, flight } = cache.begin(&newest, false) else {
            panic!("new workspace should miss");
        };
        assert!(cache.finish(&newest, generation, &flight, Some(&notes)));

        assert_eq!(
            cache
                .state
                .lock()
                .expect("cache mutex should lock")
                .entries
                .len(),
            MAX_CACHED_WORKSPACES
        );
        assert!(matches!(
            cache.begin(&recently_used, false),
            CacheLookup::Hit(_)
        ));
        let least_recently_used = Workspace::Team {
            team_path: "team-1".to_owned(),
        };
        let CacheLookup::Fill { generation, flight } = cache.begin(&least_recently_used, false)
        else {
            panic!("least-recently-used workspace should have been evicted");
        };
        let _ = cache.finish(&least_recently_used, generation, &flight, None);
    }

    #[tokio::test]
    async fn a_fill_prunes_expired_entries_for_other_workspaces() {
        let cache = NotesCache::new(Duration::from_millis(5));
        let notes: Arc<[crate::dto::NoteResponse]> = Vec::new().into();
        for team_path in ["old-a", "old-b"] {
            let workspace = Workspace::Team {
                team_path: team_path.to_owned(),
            };
            let CacheLookup::Fill { generation, flight } = cache.begin(&workspace, false) else {
                panic!("new workspace should miss");
            };
            assert!(cache.finish(&workspace, generation, &flight, Some(&notes)));
        }
        tokio::time::sleep(Duration::from_millis(10)).await;

        let current = Workspace::Personal;
        let CacheLookup::Fill { generation, flight } = cache.begin(&current, false) else {
            panic!("new workspace should miss");
        };
        assert!(cache.finish(&current, generation, &flight, Some(&notes)));
        let state = cache.state.lock().expect("cache mutex should lock");
        assert_eq!(state.entries.len(), 1);
        assert!(state.entries.contains_key(&Workspace::Personal));
    }

    #[tokio::test]
    async fn a_write_that_fails_still_drops_the_cached_list() {
        // HackMD applies writes asynchronously, so a PATCH that answers with an
        // error may still have landed. The cache has to go either way.
        let server =
            SequenceServer::spawn([(200, NOTES), (400, r#"{"error":"nope"}"#), (200, NOTES)]);
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

        assert_eq!(server.finish().len(), 3);
    }

    #[tokio::test]
    async fn team_and_personal_lists_do_not_share_a_cache_entry() {
        let server = SequenceServer::spawn([(200, NOTES), (200, NOTES)]);
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

        let requests = server.finish();
        assert!(requests[0].starts_with("GET /v1/notes HTTP/1.1\r\n"));
        assert!(requests[1].starts_with("GET /v1/teams/core/notes HTTP/1.1\r\n"));
    }

    #[tokio::test]
    async fn encodes_each_path_segment_and_attaches_bearer_auth() {
        const TOKEN: &str = "fixture-bearer-token";
        let server = SequenceServer::spawn([(200, r#"{"id":"ok"}"#)]);
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
        let request = server.finish_one();
        assert!(request.starts_with("GET /v1/teams/team%2Fpath/notes/note%20%3F%23 HTTP/1.1\r\n"));
        assert!(
            request
                .to_ascii_lowercase()
                .contains("authorization: bearer fixture-bearer-token\r\n")
        );
    }

    #[tokio::test]
    async fn sends_json_payload_and_parses_response_once() {
        let server = SequenceServer::spawn([(200, r#"{"saved":true}"#)]);
        let client = server.client();
        let payload = json!({"title": "hello"});

        let response = client
            .request_json::<Value>(Method::PATCH, &["notes", "id"], Some(&payload))
            .await
            .expect("fixture request should succeed");
        assert_eq!(response, Some(json!({"saved": true})));
        let request = server.finish_one();
        assert!(request.starts_with("PATCH /v1/notes/id HTTP/1.1\r\n"));
        assert!(request.ends_with(r#"{"title":"hello"}"#));
    }

    #[tokio::test]
    async fn omitted_optional_fields_remain_omitted_on_the_wire() {
        let server = SequenceServer::spawn([(204, "")]);
        let client = server.client();
        let payload = serde_json::to_value(CreateNoteRequest::default())
            .expect("typed payload should serialize");

        let response = client
            .request_json::<Value>(Method::POST, &["notes"], Some(&payload))
            .await
            .expect("fixture request should succeed");
        assert_eq!(response, None);
        let request = server.finish_one();
        assert!(request.starts_with("POST /v1/notes HTTP/1.1\r\n"));
        assert!(request.ends_with("{}"));
        assert!(!request.contains("readPermission"));
        assert!(!request.contains("writePermission"));
        assert!(!request.contains("commentPermission"));
    }

    #[tokio::test]
    async fn accepts_empty_202_and_204_responses() {
        for status in [202, 204] {
            let server = SequenceServer::spawn([(status, "")]);
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
        let create_server = SequenceServer::spawn([(201, created)]);
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
        let request = create_server.finish_one();
        assert!(request.starts_with("POST /v1/teams/team%2Fpath/notes HTTP/1.1\r\n"));
        assert!(request.ends_with(r#"{"title":"New"}"#));

        let update_server = SequenceServer::spawn([(202, "")]);
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
        let request = update_server.finish_one();
        assert!(request.starts_with("PATCH /v1/notes/note%2Fid HTTP/1.1\r\n"));
        assert!(request.ends_with(r#"{"parentFolderId":null}"#));

        let delete_server = SequenceServer::spawn([(204, "")]);
        let delete_client = delete_server.client();
        let response = delete_client
            .delete_note(
                &Workspace::Team {
                    team_path: "team".to_owned(),
                },
                "note-id",
            )
            .await
            .expect("team note should be deleted");
        assert_eq!(response, None);
        assert!(
            delete_server
                .finish_one()
                .starts_with("DELETE /v1/teams/team/notes/note-id HTTP/1.1\r\n")
        );
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
            let server = SequenceServer::spawn([(status, r#"{"error":"fixture"}"#)]);
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
        let server = crate::fixture::SequenceServer::spawn_with_headers([(
            429,
            r#"{"error":"slow down"}"#,
            HEADERS,
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
        let server = SequenceServer::spawn([(503, &body)]);
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
        let server = SequenceServer::spawn([(400, &body)]);
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
        let server = SequenceServer::spawn([(200, "not-json-sensitive-content")]);
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

        let server =
            SequenceServer::spawn_delayed(200, r#"{"ok":true}"#, Duration::from_millis(100));
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
        let profile_server = SequenceServer::spawn([(
            200,
            r#"{"id":"user-id","name":"Alice","email":"alice@example.test","userPath":"alice","photo":null,"teams":[]}"#,
        )]);
        let profile_client = profile_server.client();
        let profile = profile_client
            .get_me()
            .await
            .expect("profile should deserialize");
        assert_eq!(profile.user_path, "alice");
        assert!(
            profile_server
                .finish_one()
                .starts_with("GET /v1/me HTTP/1.1\r\n")
        );

        let teams_server = SequenceServer::spawn([(
            200,
            r#"[{"id":"team-id","name":"Engineering","path":"engineering","description":null,"hardLimit":100,"visibility":"private"}]"#,
        )]);
        let teams_client = teams_server.client();
        let teams = teams_client
            .list_teams()
            .await
            .expect("teams should deserialize");
        assert_eq!(teams[0].path, "engineering");
        assert!(
            teams_server
                .finish_one()
                .starts_with("GET /v1/teams HTTP/1.1\r\n")
        );
    }

    #[tokio::test]
    async fn retries_only_gets_and_explicitly_idempotent_patches() {
        const RETRY_AFTER: &[(&str, &str)] = &[("Retry-After", "0")];
        let get_server = crate::fixture::SequenceServer::spawn_with_headers([
            (500, r#"{"error":"transient"}"#, &[]),
            (429, r#"{"error":"rate"}"#, RETRY_AFTER),
            (200, r#"{"ok":true}"#, &[]),
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
        assert_eq!(get_server.finish().len(), 3);

        let patch_server =
            crate::fixture::SequenceServer::spawn([(500, r#"{"error":"transient"}"#), (202, "")]);
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
        assert_eq!(patch_server.finish().len(), 2);

        let post_server =
            crate::fixture::SequenceServer::spawn([(500, r#"{"error":"do not retry"}"#)]);
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
        assert_eq!(post_server.finish().len(), 1);
    }

    #[tokio::test]
    async fn readback_policy_bounds_the_fetch_itself() {
        let result = tokio::time::timeout(
            Duration::from_secs(1),
            super::poll_readback_with_policy(
                std::future::pending::<Result<Value, HackmdError>>,
                |_| true,
                Duration::from_millis(10),
                Duration::from_millis(1),
            ),
        )
        .await
        .expect("the readback policy must bound a stuck fetch");

        assert!(matches!(result, Err(HackmdError::ReadbackTimeout)));
    }

    #[tokio::test]
    async fn readback_policy_returns_the_last_bounded_observation() {
        let result = super::poll_readback_with_policy(
            || std::future::ready(Ok::<_, HackmdError>("old")),
            |value| *value == "new",
            Duration::from_millis(10),
            Duration::from_millis(20),
        )
        .await
        .expect("a completed fetch should remain observable");

        assert_eq!(result.value, "old");
        assert!(!result.confirmed);
    }

    #[test]
    fn api_errors_convert_to_caller_visible_tool_errors() {
        let result: rmcp::model::CallToolResult = HackmdError::Unauthorized {
            method: "GET".to_owned(),
            path: "/v1/me".to_owned(),
        }
        .into();

        assert_eq!(result.is_error, Some(true));
        assert!(
            result.content[0]
                .as_text()
                .expect("tool error should contain text")
                .text
                .contains("401 unauthorized")
        );
    }
}
