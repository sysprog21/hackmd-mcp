use reqwest::{Method, StatusCode};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::Path;
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
    state::StateStore,
};

/// HTTP client shared by all `HackMD` tool handlers.
#[derive(Debug)]
pub(crate) struct HackmdClient {
    config: Config,
    http: reqwest::Client,
    #[allow(dead_code, reason = "used by the local sync tool tasks")]
    state: StateStore,
}

impl HackmdClient {
    pub(crate) fn new(config: Config) -> Result<Self, HackmdError> {
        let http = reqwest::Client::builder()
            .connect_timeout(config.connect_timeout())
            .timeout(config.request_timeout())
            .build()
            .map_err(|_| HackmdError::ClientBuild)?;
        let state = StateStore::new(config.state_dir().to_path_buf());
        Ok(Self {
            config,
            http,
            state,
        })
    }

    pub(crate) fn has_api_token(&self) -> bool {
        self.config.has_api_token()
    }

    pub(crate) async fn get_me(&self) -> Result<ProfileResponse, HackmdError> {
        let path = self.url_for_segments(&["me"])?.path().to_owned();
        self.request_json(Method::GET, &["me"], None)
            .await?
            .ok_or_else(|| HackmdError::EmptyResponse {
                method: "GET".to_owned(),
                path,
            })
    }

    pub(crate) async fn list_teams(&self) -> Result<Vec<TeamResponse>, HackmdError> {
        let path = self.url_for_segments(&["teams"])?.path().to_owned();
        self.request_json(Method::GET, &["teams"], None)
            .await?
            .ok_or_else(|| HackmdError::EmptyResponse {
                method: "GET".to_owned(),
                path,
            })
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
        let path = self.url_for_segments(&["history"])?.path().to_owned();
        self.request_json::<HistoryResponse>(Method::GET, &["history"], None)
            .await?
            .map(HistoryResponse::into_notes)
            .ok_or_else(|| HackmdError::EmptyResponse {
                method: "GET".to_owned(),
                path,
            })
    }

    #[allow(dead_code, reason = "used by note resolution and list-note tool tasks")]
    pub(crate) async fn list_notes(
        &self,
        workspace: &Workspace,
    ) -> Result<Vec<NoteResponse>, HackmdError> {
        let segments: Vec<&str> = match workspace {
            Workspace::Personal => vec!["notes"],
            Workspace::Team { team_path } => vec!["teams", team_path, "notes"],
        };
        let path = self.url_for_segments(&segments)?.path().to_owned();
        self.request_json(Method::GET, &segments, None)
            .await?
            .ok_or_else(|| HackmdError::EmptyResponse {
                method: "GET".to_owned(),
                path,
            })
    }

    pub(crate) async fn get_note(
        &self,
        workspace: &Workspace,
        note_id: &str,
    ) -> Result<NoteResponse, HackmdError> {
        let segments: Vec<&str> = match workspace {
            Workspace::Personal => vec!["notes", note_id],
            Workspace::Team { team_path } => vec!["teams", team_path, "notes", note_id],
        };
        let path = self.url_for_segments(&segments)?.path().to_owned();
        self.request_json(Method::GET, &segments, None)
            .await?
            .ok_or_else(|| HackmdError::EmptyResponse {
                method: "GET".to_owned(),
                path,
            })
    }

    pub(crate) async fn create_note(
        &self,
        workspace: &Workspace,
        payload: &CreateNoteRequest,
    ) -> Result<NoteResponse, HackmdError> {
        let segments: Vec<&str> = match workspace {
            Workspace::Personal => vec!["notes"],
            Workspace::Team { team_path } => vec!["teams", team_path, "notes"],
        };
        let path = self.url_for_segments(&segments)?.path().to_owned();
        let body = serde_json::to_value(payload).map_err(|_| HackmdError::InvalidPayload)?;
        self.request_json(Method::POST, &segments, Some(&body))
            .await?
            .ok_or_else(|| HackmdError::EmptyResponse {
                method: "POST".to_owned(),
                path,
            })
    }

    pub(crate) async fn update_note(
        &self,
        workspace: &Workspace,
        note_id: &str,
        payload: &UpdateNoteRequest,
    ) -> Result<Option<NoteResponse>, HackmdError> {
        let segments: Vec<&str> = match workspace {
            Workspace::Personal => vec!["notes", note_id],
            Workspace::Team { team_path } => vec!["teams", team_path, "notes", note_id],
        };
        let body = serde_json::to_value(payload).map_err(|_| HackmdError::InvalidPayload)?;
        self.request_json(Method::PATCH, &segments, Some(&body))
            .await
    }

    pub(crate) async fn delete_note(
        &self,
        workspace: &Workspace,
        note_id: &str,
    ) -> Result<Option<Value>, HackmdError> {
        let segments: Vec<&str> = match workspace {
            Workspace::Personal => vec!["notes", note_id],
            Workspace::Team { team_path } => vec!["teams", team_path, "notes", note_id],
        };
        self.request_json(Method::DELETE, &segments, None).await
    }

    pub(crate) async fn list_folders(
        &self,
        workspace: &Workspace,
    ) -> Result<Vec<FolderResponse>, HackmdError> {
        let segments: Vec<&str> = match workspace {
            Workspace::Personal => vec!["folders"],
            Workspace::Team { team_path } => vec!["teams", team_path, "folders"],
        };
        let path = self.url_for_segments(&segments)?.path().to_owned();
        self.request_json(Method::GET, &segments, None)
            .await?
            .ok_or_else(|| HackmdError::EmptyResponse {
                method: "GET".to_owned(),
                path,
            })
    }

    pub(crate) async fn get_folder(
        &self,
        workspace: &Workspace,
        folder_id: &str,
    ) -> Result<FolderResponse, HackmdError> {
        let segments: Vec<&str> = match workspace {
            Workspace::Personal => vec!["folders", folder_id],
            Workspace::Team { team_path } => vec!["teams", team_path, "folders", folder_id],
        };
        let path = self.url_for_segments(&segments)?.path().to_owned();
        self.request_json(Method::GET, &segments, None)
            .await?
            .ok_or_else(|| HackmdError::EmptyResponse {
                method: "GET".to_owned(),
                path,
            })
    }

    pub(crate) async fn create_folder(
        &self,
        workspace: &Workspace,
        payload: &CreateFolderRequest,
    ) -> Result<FolderResponse, HackmdError> {
        let segments: Vec<&str> = match workspace {
            Workspace::Personal => vec!["folders"],
            Workspace::Team { team_path } => vec!["teams", team_path, "folders"],
        };
        let path = self.url_for_segments(&segments)?.path().to_owned();
        let body = serde_json::to_value(payload).map_err(|_| HackmdError::InvalidPayload)?;
        self.request_json(Method::POST, &segments, Some(&body))
            .await?
            .ok_or_else(|| HackmdError::EmptyResponse {
                method: "POST".to_owned(),
                path,
            })
    }

    pub(crate) async fn update_folder(
        &self,
        workspace: &Workspace,
        folder_id: &str,
        payload: &UpdateFolderRequest,
    ) -> Result<Option<FolderResponse>, HackmdError> {
        let segments: Vec<&str> = match workspace {
            Workspace::Personal => vec!["folders", folder_id],
            Workspace::Team { team_path } => vec!["teams", team_path, "folders", folder_id],
        };
        let body = serde_json::to_value(payload).map_err(|_| HackmdError::InvalidPayload)?;
        self.request_json(Method::PATCH, &segments, Some(&body))
            .await
    }

    pub(crate) async fn delete_folder(
        &self,
        workspace: &Workspace,
        folder_id: &str,
    ) -> Result<Option<Value>, HackmdError> {
        let segments: Vec<&str> = match workspace {
            Workspace::Personal => vec!["folders", folder_id],
            Workspace::Team { team_path } => vec!["teams", team_path, "folders", folder_id],
        };
        self.request_json(Method::DELETE, &segments, None).await
    }

    pub(crate) async fn get_folder_order(
        &self,
        workspace: &Workspace,
    ) -> Result<BTreeMap<String, Vec<String>>, HackmdError> {
        let segments: Vec<&str> = match workspace {
            Workspace::Personal => vec!["folders", "folder-order"],
            Workspace::Team { team_path } => {
                vec!["teams", team_path, "folders", "folder-order"]
            }
        };
        let path = self.url_for_segments(&segments)?.path().to_owned();
        self.request_json(Method::GET, &segments, None)
            .await?
            .ok_or_else(|| HackmdError::EmptyResponse {
                method: "GET".to_owned(),
                path,
            })
    }

    pub(crate) async fn set_folder_order(
        &self,
        workspace: &Workspace,
        order: &BTreeMap<String, Vec<String>>,
    ) -> Result<Option<Value>, HackmdError> {
        let segments: Vec<&str> = match workspace {
            Workspace::Personal => vec!["folders", "folder-order"],
            Workspace::Team { team_path } => {
                vec!["teams", team_path, "folders", "folder-order"]
            }
        };
        self.request_json(Method::PUT, &segments, Some(&json!({"order": order})))
            .await
    }

    pub(crate) async fn upload_note_image(
        &self,
        note_id: &str,
        image_path: &Path,
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
        let form = reqwest::multipart::Form::new()
            .file("image", image_path)
            .await
            .map_err(|_| HackmdError::ImageRead)?;
        let response = self
            .http
            .post(url)
            .bearer_auth(token)
            .multipart(form)
            .send()
            .await
            .map_err(|error| request_error(&error, "POST".to_owned(), path.clone()))?;
        let status = response.status();
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
            ));
        }
        serde_json::from_slice(&bytes).map_err(|_| HackmdError::InvalidJson {
            method: "POST".to_owned(),
            path,
            status,
        })
    }

    #[allow(dead_code, reason = "called by the API operation tasks")]
    pub(crate) async fn request_json<T: DeserializeOwned>(
        &self,
        method: Method,
        path_segments: &[&str],
        body: Option<&Value>,
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
        let mut request = self.http.request(method, url).bearer_auth(token);
        if let Some(body) = body {
            request = request.json(body);
        }

        let response = request
            .send()
            .await
            .map_err(|error| request_error(&error, method_text.clone(), path.clone()))?;
        let status = response.status();
        let bytes = response
            .bytes()
            .await
            .map_err(|error| request_error(&error, method_text.clone(), path.clone()))?;

        if !status.is_success() {
            return Err(map_status_error(status, method_text, path, &bytes, token));
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

fn request_error(error: &reqwest::Error, method: String, path: String) -> HackmdError {
    if error.is_timeout() {
        HackmdError::Timeout { method, path }
    } else {
        HackmdError::Network { method, path }
    }
}

fn map_status_error(
    status: StatusCode,
    method: String,
    path: String,
    body: &[u8],
    token: &str,
) -> HackmdError {
    match status {
        StatusCode::UNAUTHORIZED => HackmdError::Unauthorized { method, path },
        StatusCode::FORBIDDEN => HackmdError::Forbidden { method, path },
        StatusCode::NOT_FOUND => HackmdError::NotFound { method, path },
        StatusCode::CONFLICT => HackmdError::Conflict { method, path },
        StatusCode::TOO_MANY_REQUESTS => HackmdError::RateLimited { method, path },
        status if status.is_server_error() => HackmdError::Upstream {
            method,
            path,
            status,
        },
        status => HackmdError::Api {
            method,
            path,
            status,
            detail: bounded_body(body, token),
        },
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
    #[error("configured HACKMD_API_URL cannot be used as an API base URL")]
    InvalidBaseUrl,
    #[error("failed to serialize a validated HackMD request payload")]
    InvalidPayload,
    #[error("image file could not be opened for upload")]
    ImageRead,
    #[error(
        "POST {path}: HackMD rejected the image as too large (413); resize it below 5 MB and retry"
    )]
    ImageTooLarge { path: String },
    #[error("team workspace {team_path:?} is not available to this HackMD account")]
    UnknownTeam { team_path: String },
    #[error("{method} {path}: request timed out; check network connectivity and retry")]
    Timeout { method: String, path: String },
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
    #[error(
        "{method} {path}: 429 rate limited (100 requests per 5 minutes; monthly quota 2,000 free / 20,000 Prime); wait before retrying"
    )]
    RateLimited { method: String, path: String },
    #[error("{method} {path}: upstream HackMD error ({status}); retry later")]
    Upstream {
        method: String,
        path: String,
        status: StatusCode,
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
        crate::tool_result::error(error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use std::{
        io::{Read, Write},
        net::{TcpListener, TcpStream},
        sync::mpsc::{self, Receiver},
        thread::{self, JoinHandle},
        time::Duration,
    };

    use reqwest::Method;
    use serde_json::{Value, json};

    use super::{HackmdClient, HackmdError};
    use crate::config::Config;
    use crate::{
        dto::{CreateNoteRequest, UpdateNoteRequest},
        models::Workspace,
    };

    struct FixtureServer {
        api_url: String,
        request: Receiver<String>,
        thread: JoinHandle<()>,
    }

    impl FixtureServer {
        fn spawn(status: u16, body: &str) -> Self {
            Self::spawn_delayed(status, body, Duration::ZERO)
        }

        fn spawn_delayed(status: u16, body: &str, delay: Duration) -> Self {
            let listener =
                TcpListener::bind("127.0.0.1:0").expect("fixture listener should bind to loopback");
            let address = listener
                .local_addr()
                .expect("fixture address should be available");
            let api_url = format!("http://{address}/v1");
            let body = body.to_owned();
            let (sender, request) = mpsc::channel();
            let thread = thread::spawn(move || {
                let (mut stream, _) = listener.accept().expect("fixture should accept a request");
                let request_bytes = read_request(&mut stream);
                sender
                    .send(String::from_utf8_lossy(&request_bytes).into_owned())
                    .expect("fixture request should be captured");
                thread::sleep(delay);
                let response = format!(
                    "HTTP/1.1 {status} Fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _result = stream.write_all(response.as_bytes());
            });
            Self {
                api_url,
                request,
                thread,
            }
        }

        fn finish(self) -> String {
            let request = self
                .request
                .recv_timeout(Duration::from_secs(2))
                .expect("fixture should capture one request");
            self.thread.join().expect("fixture thread should finish");
            request
        }
    }

    fn read_request(stream: &mut TcpStream) -> Vec<u8> {
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("fixture read timeout should be set");
        let mut request = Vec::new();
        let mut buffer = [0_u8; 4096];
        loop {
            let count = stream
                .read(&mut buffer)
                .expect("request should be readable");
            if count == 0 {
                break;
            }
            request.extend_from_slice(&buffer[..count]);
            if let Some(header_end) = find_header_end(&request) {
                let headers = String::from_utf8_lossy(&request[..header_end]);
                let content_length = headers
                    .lines()
                    .find_map(|line| {
                        line.split_once(':').and_then(|(name, value)| {
                            name.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<usize>().ok())
                                .flatten()
                        })
                    })
                    .unwrap_or(0);
                if request.len() >= header_end + 4 + content_length {
                    break;
                }
            }
        }
        request
    }

    fn find_header_end(request: &[u8]) -> Option<usize> {
        request.windows(4).position(|window| window == b"\r\n\r\n")
    }

    fn fixture_client(server: &FixtureServer, token: &str) -> HackmdClient {
        HackmdClient::new(Config::for_loopback_test(&server.api_url, Some(token)))
            .expect("fixture client should build")
    }

    #[tokio::test]
    async fn encodes_each_path_segment_and_attaches_bearer_auth() {
        const TOKEN: &str = "fixture-bearer-token";
        let server = FixtureServer::spawn(200, r#"{"id":"ok"}"#);
        let client = fixture_client(&server, TOKEN);
        assert!(!format!("{client:?}").contains(TOKEN));

        let response = client
            .request_json::<Value>(
                Method::GET,
                &["teams", "team/path", "notes", "note ?#"],
                None,
            )
            .await
            .expect("fixture request should succeed")
            .expect("JSON response should be present");
        assert_eq!(response, json!({"id": "ok"}));
        let request = server.finish();
        assert!(request.starts_with("GET /v1/teams/team%2Fpath/notes/note%20%3F%23 HTTP/1.1\r\n"));
        assert!(
            request
                .to_ascii_lowercase()
                .contains("authorization: bearer fixture-bearer-token\r\n")
        );
    }

    #[tokio::test]
    async fn sends_json_payload_and_parses_response_once() {
        let server = FixtureServer::spawn(200, r#"{"saved":true}"#);
        let client = fixture_client(&server, "fixture-token");
        let payload = json!({"title": "hello"});

        let response = client
            .request_json::<Value>(Method::PATCH, &["notes", "id"], Some(&payload))
            .await
            .expect("fixture request should succeed");
        assert_eq!(response, Some(json!({"saved": true})));
        let request = server.finish();
        assert!(request.starts_with("PATCH /v1/notes/id HTTP/1.1\r\n"));
        assert!(request.ends_with(r#"{"title":"hello"}"#));
    }

    #[tokio::test]
    async fn omitted_optional_fields_remain_omitted_on_the_wire() {
        let server = FixtureServer::spawn(204, "");
        let client = fixture_client(&server, "fixture-token");
        let payload = serde_json::to_value(CreateNoteRequest::default())
            .expect("typed payload should serialize");

        let response = client
            .request_json::<Value>(Method::POST, &["notes"], Some(&payload))
            .await
            .expect("fixture request should succeed");
        assert_eq!(response, None);
        let request = server.finish();
        assert!(request.starts_with("POST /v1/notes HTTP/1.1\r\n"));
        assert!(request.ends_with("{}"));
        assert!(!request.contains("readPermission"));
        assert!(!request.contains("writePermission"));
        assert!(!request.contains("commentPermission"));
    }

    #[tokio::test]
    async fn accepts_empty_202_and_204_responses() {
        for status in [202, 204] {
            let server = FixtureServer::spawn(status, "");
            let client = fixture_client(&server, "fixture-token");
            let response = client
                .request_json::<Value>(Method::PATCH, &["notes", "id"], None)
                .await
                .expect("empty success should be accepted");
            assert_eq!(response, None);
            server.finish();
        }
    }

    #[tokio::test]
    async fn typed_crud_operations_use_workspace_routes_and_payloads() {
        let created = r#"{"id":"new-id","title":"New"}"#;
        let create_server = FixtureServer::spawn(201, created);
        let create_client = fixture_client(&create_server, "fixture-token");
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
        let request = create_server.finish();
        assert!(request.starts_with("POST /v1/teams/team%2Fpath/notes HTTP/1.1\r\n"));
        assert!(request.ends_with(r#"{"title":"New"}"#));

        let update_server = FixtureServer::spawn(202, "");
        let update_client = fixture_client(&update_server, "fixture-token");
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
        let request = update_server.finish();
        assert!(request.starts_with("PATCH /v1/notes/note%2Fid HTTP/1.1\r\n"));
        assert!(request.ends_with(r#"{"parentFolderId":null}"#));

        let delete_server = FixtureServer::spawn(204, "");
        let delete_client = fixture_client(&delete_server, "fixture-token");
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
                .finish()
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
            let server = FixtureServer::spawn(status, r#"{"error":"fixture"}"#);
            let client = fixture_client(&server, "fixture-token");
            let error = client
                .request_json::<Value>(Method::GET, &["notes", "id"], None)
                .await
                .expect_err("failure status should map to an error");
            let message = error.to_string();
            assert!(message.starts_with("GET /v1/notes/id:"));
            assert!(message.contains(expected), "unexpected error: {message}");
            server.finish();
        }
    }

    #[tokio::test]
    async fn bounds_generic_errors_and_redacts_the_token() {
        const TOKEN: &str = "fixture-sensitive-token";
        let body = format!("{} {TOKEN} {}", "x".repeat(280), "x".repeat(400));
        let server = FixtureServer::spawn(400, &body);
        let client = fixture_client(&server, TOKEN);

        let message = client
            .request_json::<Value>(Method::GET, &["notes"], None)
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
        let server = FixtureServer::spawn(200, "not-json-sensitive-content");
        let client = fixture_client(&server, "fixture-token");

        let message = client
            .request_json::<Value>(Method::GET, &["me"], None)
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
        let listener =
            TcpListener::bind("127.0.0.1:0").expect("temporary listener should bind to loopback");
        let address = listener
            .local_addr()
            .expect("temporary address should be available");
        drop(listener);
        let network_client = HackmdClient::new(Config::for_loopback_test(
            &format!("http://{address}/v1"),
            Some("fixture-token"),
        ))
        .expect("network fixture client should build");
        assert!(matches!(
            network_client
                .request_json::<Value>(Method::GET, &["me"], None)
                .await,
            Err(HackmdError::Network { .. })
        ));

        let server =
            FixtureServer::spawn_delayed(200, r#"{"ok":true}"#, Duration::from_millis(100));
        let timeout_config = Config::for_loopback_test_with_timeout(
            &server.api_url,
            "fixture-token",
            Duration::from_millis(20),
        );
        let timeout_client =
            HackmdClient::new(timeout_config).expect("timeout fixture client should build");
        assert!(matches!(
            timeout_client
                .request_json::<Value>(Method::GET, &["me"], None)
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
                .request_json::<Value>(Method::GET, &["me"], None)
                .await,
            Err(HackmdError::MissingToken { .. })
        ));
    }

    #[tokio::test]
    async fn typed_profile_and_team_operations_use_discovery_routes() {
        let profile_server = FixtureServer::spawn(
            200,
            r#"{"id":"user-id","name":"Alice","email":"alice@example.test","userPath":"alice","photo":null,"teams":[]}"#,
        );
        let profile_client = fixture_client(&profile_server, "fixture-token");
        let profile = profile_client
            .get_me()
            .await
            .expect("profile should deserialize");
        assert_eq!(profile.user_path, "alice");
        assert!(
            profile_server
                .finish()
                .starts_with("GET /v1/me HTTP/1.1\r\n")
        );

        let teams_server = FixtureServer::spawn(
            200,
            r#"[{"id":"team-id","name":"Engineering","path":"engineering","description":null,"hardLimit":100,"visibility":"private"}]"#,
        );
        let teams_client = fixture_client(&teams_server, "fixture-token");
        let teams = teams_client
            .list_teams()
            .await
            .expect("teams should deserialize");
        assert_eq!(teams[0].path, "engineering");
        assert!(
            teams_server
                .finish()
                .starts_with("GET /v1/teams HTTP/1.1\r\n")
        );
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
