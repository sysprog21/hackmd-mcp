//! Destructive live API smoke tests.
//!
//! These tests are ignored by default. Run explicitly with a dedicated test
//! account/token and isolated team:
//! `HACKMD_RUN_LIVE_TESTS=1 HACKMD_LIVE_TEST_TOKEN=... HACKMD_LIVE_TEST_TEAM_PATH=... cargo test --test live-smoke -- --ignored`

use std::time::{SystemTime, UNIX_EPOCH};

use futures_util::FutureExt;
use reqwest::{Method, StatusCode, Url};
use serde_json::{Value, json};

struct LiveApi {
    http: reqwest::Client,
    base: Url,
    token: String,
}

impl LiveApi {
    fn from_env() -> Self {
        assert_eq!(
            std::env::var("HACKMD_RUN_LIVE_TESTS").as_deref(),
            Ok("1"),
            "set HACKMD_RUN_LIVE_TESTS=1 to acknowledge destructive live tests"
        );
        let token = std::env::var("HACKMD_LIVE_TEST_TOKEN")
            .expect("set a dedicated HACKMD_LIVE_TEST_TOKEN; the normal server token is refused");
        assert!(
            !token.trim().is_empty(),
            "live test token must not be empty"
        );
        let base = std::env::var("HACKMD_LIVE_TEST_API_URL")
            .unwrap_or_else(|_| "https://api.hackmd.io/v1".to_owned());
        let base = Url::parse(&base).expect("HACKMD_LIVE_TEST_API_URL must be a valid URL");
        assert_eq!(base.scheme(), "https", "live API URL must use HTTPS");
        Self {
            http: reqwest::Client::builder()
                .connect_timeout(std::time::Duration::from_secs(10))
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .expect("live HTTP client should build"),
            base,
            token,
        }
    }

    fn url(&self, segments: &[&str]) -> Url {
        let mut url = self.base.clone();
        let mut path = url
            .path_segments_mut()
            .expect("HTTPS live API URL must support path segments");
        path.pop_if_empty();
        path.extend(segments);
        drop(path);
        url
    }

    async fn request(
        &self,
        method: Method,
        segments: &[&str],
        body: Option<&Value>,
    ) -> (StatusCode, Option<Value>) {
        let mut request = self
            .http
            .request(method, self.url(segments))
            .bearer_auth(&self.token);
        if let Some(body) = body {
            request = request.json(body);
        }
        let response = request
            .send()
            .await
            .expect("live API request should complete");
        let status = response.status();
        let bytes = response
            .bytes()
            .await
            .expect("live response body should read");
        let value = (!bytes.is_empty()).then(|| {
            serde_json::from_slice(&bytes)
                .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()))
        });
        (status, value)
    }

    async fn json(&self, method: Method, segments: &[&str], body: Option<&Value>) -> Value {
        let (status, value) = self.request(method, segments, body).await;
        assert!(status.is_success(), "live API returned {status}: {value:?}");
        value.expect("live API response should contain JSON")
    }

    async fn empty_ok(&self, method: Method, segments: &[&str], body: Option<&Value>) {
        let (status, value) = self.request(method, segments, body).await;
        assert!(status.is_success(), "live API returned {status}: {value:?}");
    }

    async fn delete_note(&self, note_id: &str) {
        self.empty_ok(Method::DELETE, &["notes", note_id], None)
            .await;
    }
}

#[derive(Default)]
struct Fixtures {
    note: Option<String>,
    child_folder: Option<String>,
    parent_folder: Option<String>,
}

async fn cleanup(api: &LiveApi, fixtures: &mut Fixtures) {
    if let Some(note_id) = fixtures.note.take() {
        let _ = api
            .request(Method::DELETE, &["notes", &note_id], None)
            .await;
    }
    if let Some(folder_id) = fixtures.child_folder.take() {
        let _ = api
            .request(Method::DELETE, &["folders", &folder_id], None)
            .await;
    }
    if let Some(folder_id) = fixtures.parent_folder.take() {
        let _ = api
            .request(Method::DELETE, &["folders", &folder_id], None)
            .await;
    }
}

#[tokio::test]
#[ignore = "requires explicit destructive HackMD live-test environment"]
#[allow(
    clippy::too_many_lines,
    reason = "one linear live workflow keeps fixture creation, assertions, and cleanup ordering auditable"
)]
async fn personal_crud_folder_order_trash_and_restore() {
    let api = LiveApi::from_env();
    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock should be after epoch")
        .as_millis();
    let mut fixtures = Fixtures::default();

    let outcome = std::panic::AssertUnwindSafe(async {
        let profile = api.json(Method::GET, &["me"], None).await;
        assert!(profile["id"].is_string(), "profile must carry an ID");

        let parent = api
            .json(
                Method::POST,
                &["folders"],
                Some(&json!({"name": format!("codex-live-{suffix}")})),
            )
            .await;
        let parent_id = parent["id"].as_str().expect("parent folder ID").to_owned();
        fixtures.parent_folder = Some(parent_id.clone());

        let child = api
            .json(
                Method::POST,
                &["folders"],
                Some(&json!({
                    "name": format!("codex-live-child-{suffix}"),
                    "parentFolderId": parent_id
                })),
            )
            .await;
        let child_id = child["id"].as_str().expect("child folder ID").to_owned();
        fixtures.child_folder = Some(child_id.clone());

        let content = format!("# Codex live {suffix}\n\ninitial\n");
        let created = api
            .json(
                Method::POST,
                &["notes"],
                Some(&json!({
                    "title": format!("codex-live-{suffix}"),
                    "content": content,
                    "parentFolderId": child_id
                })),
            )
            .await;
        let note_id = created["id"].as_str().expect("created note ID").to_owned();
        fixtures.note = Some(note_id.clone());

        let post_read = api.json(Method::GET, &["notes", &note_id], None).await;
        let post_assigned_folder = post_read["folderPaths"]
            .as_array()
            .is_some_and(|paths| paths.iter().any(|folder| folder["id"] == child_id));
        assert!(
            post_assigned_folder,
            "note POST did not preserve the requested folder assignment"
        );

        let edited = format!("# Codex live {suffix}\n\nedited\n");
        api.empty_ok(
            Method::PATCH,
            &["notes", &note_id],
            Some(&json!({"content": edited})),
        )
        .await;
        let read = api.json(Method::GET, &["notes", &note_id], None).await;
        assert_eq!(read["content"], edited);
        api.empty_ok(
            Method::PATCH,
            &["notes", &note_id],
            Some(&json!({"content": edited})),
        )
        .await;
        let unchanged = api.json(Method::GET, &["notes", &note_id], None).await;
        assert_eq!(unchanged["content"], edited);

        let order = api
            .json(Method::GET, &["folders", "folder-order"], None)
            .await;
        api.empty_ok(
            Method::PUT,
            &["folders", "folder-order"],
            Some(&json!({"order": order})),
        )
        .await;

        api.delete_note(&note_id).await;
        let trash = api.json(Method::GET, &["trash"], None).await;
        let was_trashed = trash
            .as_array()
            .is_some_and(|notes| notes.iter().any(|note| note["id"] == note_id));
        assert!(
            was_trashed,
            "DELETE /notes did not place the note in GET /trash"
        );
        api.empty_ok(Method::PUT, &["trash", &note_id, "restore"], None)
            .await;
        let restored = api.json(Method::GET, &["notes", &note_id], None).await;
        assert_eq!(restored["id"], note_id);
    })
    .catch_unwind()
    .await;

    cleanup(&api, &mut fixtures).await;
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}

#[tokio::test]
#[ignore = "requires explicit destructive HackMD live-test team environment"]
#[allow(clippy::too_many_lines, reason = "one guarded team contract probe")]
async fn team_folder_updates_and_image_route_are_measured() {
    let api = LiveApi::from_env();
    let team_path = std::env::var("HACKMD_LIVE_TEST_TEAM_PATH")
        .expect("set an isolated HACKMD_LIVE_TEST_TEAM_PATH for the team probe");
    assert!(!team_path.trim().is_empty());
    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock should be after epoch")
        .as_millis();
    let mut note_id = None;
    let mut folder_ids = Vec::new();

    let outcome = std::panic::AssertUnwindSafe(async {
        let parent = api
            .json(
                Method::POST,
                &["teams", &team_path, "folders"],
                Some(&json!({"name": format!("codex-live-parent-{suffix}")})),
            )
            .await;
        let parent_id = parent["id"].as_str().expect("team parent ID").to_owned();
        folder_ids.push(parent_id.clone());
        let destination = api
            .json(
                Method::POST,
                &["teams", &team_path, "folders"],
                Some(&json!({"name": format!("codex-live-destination-{suffix}")})),
            )
            .await;
        let destination_id = destination["id"]
            .as_str()
            .expect("team destination ID")
            .to_owned();
        folder_ids.push(destination_id.clone());
        let child = api
            .json(
                Method::POST,
                &["teams", &team_path, "folders"],
                Some(&json!({
                    "name": format!("codex-live-child-{suffix}"),
                    "parentFolderId": parent_id
                })),
            )
            .await;
        let child_id = child["id"].as_str().expect("team child ID").to_owned();
        folder_ids.push(child_id.clone());

        api.empty_ok(
            Method::PATCH,
            &["teams", &team_path, "folders", &child_id],
            Some(&json!({"parentFolderId": destination_id})),
        )
        .await;
        let moved = api
            .json(
                Method::GET,
                &["teams", &team_path, "folders", &child_id],
                None,
            )
            .await;
        assert_eq!(
            moved["parentFolderId"], parent_id,
            "team folder PATCH unexpectedly began applying parentFolderId"
        );

        api.empty_ok(
            Method::PATCH,
            &["teams", &team_path, "folders", &child_id],
            Some(&json!({"parentFolderId": null})),
        )
        .await;
        let rooted = api
            .json(
                Method::GET,
                &["teams", &team_path, "folders", &child_id],
                None,
            )
            .await;
        assert_eq!(rooted["parentFolderId"], parent_id);

        let renamed = format!("codex-live-renamed-{suffix}");
        api.empty_ok(
            Method::PATCH,
            &["teams", &team_path, "folders", &child_id],
            Some(&json!({"name": renamed})),
        )
        .await;
        let updated = api
            .json(
                Method::GET,
                &["teams", &team_path, "folders", &child_id],
                None,
            )
            .await;
        assert_eq!(updated["name"], renamed);

        let created = api
            .json(
                Method::POST,
                &["teams", &team_path, "notes"],
                Some(&json!({
                    "title": format!("codex-live-team-image-{suffix}"),
                    "content": "# image probe"
                })),
            )
            .await;
        let created_note_id = created["id"].as_str().expect("team note ID").to_owned();
        note_id = Some(created_note_id.clone());
        let part = reqwest::multipart::Part::bytes(vec![
            137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0, 1,
            8, 6, 0, 0, 0, 31, 21, 196, 137, 0, 0, 0, 13, 73, 68, 65, 84, 8, 215, 99, 248, 207,
            192, 240, 31, 0, 5, 0, 1, 255, 137, 153, 61, 29, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66,
            96, 130,
        ])
        .file_name("probe.png")
        .mime_str("image/png")
        .expect("static MIME type should parse");
        let status = api
            .http
            .post(api.url(&["teams", &team_path, "notes", &created_note_id, "images"]))
            .bearer_auth(&api.token)
            .multipart(reqwest::multipart::Form::new().part("image", part))
            .send()
            .await
            .expect("team upload probe should complete")
            .status();
        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "team image route unexpectedly exists; production support must be revisited"
        );
    })
    .catch_unwind()
    .await;

    if let Some(note_id) = note_id {
        let _ = api
            .request(
                Method::DELETE,
                &["teams", &team_path, "notes", &note_id],
                None,
            )
            .await;
    }
    for folder_id in folder_ids.into_iter().rev() {
        let _ = api
            .request(
                Method::DELETE,
                &["teams", &team_path, "folders", &folder_id],
                None,
            )
            .await;
    }
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}
