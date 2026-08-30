//! Read-only live `HackMD` contract and conditional-request probe.
//!
//! This test never creates, edits, or deletes data. Run it manually with a
//! dedicated token and preserve `--nocapture` output as the validator evidence:
//!
//! ```sh
//! HACKMD_RUN_LIVE_READONLY_TESTS=1 \
//!     HACKMD_LIVE_TEST_TOKEN=... \
//!     cargo test --test live-readonly -- --ignored --nocapture
//! ```

use reqwest::{StatusCode, Url, header::HeaderMap};
use serde_json::Value;

struct LiveReadOnlyApi {
    http: reqwest::Client,
    base: Url,
    token: String,
}

struct LiveResponse {
    status: StatusCode,
    headers: HeaderMap,
    value: Option<Value>,
}

impl LiveReadOnlyApi {
    fn from_env() -> Self {
        assert_eq!(
            std::env::var("HACKMD_RUN_LIVE_READONLY_TESTS").as_deref(),
            Ok("1"),
            "set HACKMD_RUN_LIVE_READONLY_TESTS=1 to run the read-only live probe"
        );
        let token = std::env::var("HACKMD_LIVE_TEST_TOKEN")
            .expect("set a dedicated HACKMD_LIVE_TEST_TOKEN");
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

    async fn get(&self, segments: &[&str], conditional: Option<(&str, &str)>) -> LiveResponse {
        let mut request = self.http.get(self.url(segments)).bearer_auth(&self.token);
        if let Some((name, value)) = conditional {
            request = request.header(name, value);
        }
        let response = request.send().await.expect("live GET should complete");
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = response
            .bytes()
            .await
            .expect("live response body should read");
        let value = (!bytes.is_empty()).then(|| {
            serde_json::from_slice(&bytes)
                .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()))
        });
        LiveResponse {
            status,
            headers,
            value,
        }
    }
}

fn header_text<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|value| value.to_str().ok())
}

async fn probe_endpoint(api: &LiveReadOnlyApi, label: &str, segments: &[&str]) -> LiveResponse {
    let first = api.get(segments, None).await;
    assert!(
        first.status.is_success(),
        "{label} returned {}",
        first.status
    );
    let etag = header_text(&first.headers, "etag");
    let modified = header_text(&first.headers, "last-modified");
    eprintln!(
        "{label}: status={} etag={etag:?} last_modified={modified:?}",
        first.status
    );

    let conditional = etag
        .map(|value| ("if-none-match", value))
        .or_else(|| modified.map(|value| ("if-modified-since", value)));
    if let Some(validator) = conditional {
        let second = api.get(segments, Some(validator)).await;
        eprintln!(
            "{label}: conditional_status={} validator={}",
            second.status, validator.0
        );
        assert!(
            second.status == StatusCode::NOT_MODIFIED || second.status.is_success(),
            "{label} conditional GET returned unexpected {}",
            second.status
        );
    } else {
        eprintln!("{label}: conditional probe skipped; no validator header");
    }
    first
}

#[tokio::test]
#[ignore = "requires explicit read-only HackMD live-test environment"]
async fn personal_team_and_note_gets_report_validator_support() {
    let api = LiveReadOnlyApi::from_env();
    let profile = probe_endpoint(&api, "personal profile", &["me"]).await;
    assert!(
        profile
            .value
            .as_ref()
            .is_some_and(|value| value["id"].is_string()),
        "profile must carry an ID"
    );

    let notes = probe_endpoint(&api, "personal notes", &["notes"]).await;
    if let Some(note_id) = notes
        .value
        .as_ref()
        .and_then(Value::as_array)
        .and_then(|notes| notes.first())
        .and_then(|note| note["id"].as_str())
    {
        probe_endpoint(&api, "personal note", &["notes", note_id]).await;
    } else {
        eprintln!("personal note: skipped; account has no notes");
    }

    probe_endpoint(&api, "teams", &["teams"]).await;
    if let Ok(team_path) = std::env::var("HACKMD_LIVE_TEST_TEAM_PATH") {
        assert!(!team_path.trim().is_empty(), "team path must not be empty");
        let team_notes = probe_endpoint(&api, "team notes", &["teams", &team_path, "notes"]).await;
        if let Some(note_id) = team_notes
            .value
            .as_ref()
            .and_then(Value::as_array)
            .and_then(|notes| notes.first())
            .and_then(|note| note["id"].as_str())
        {
            probe_endpoint(&api, "team note", &["teams", &team_path, "notes", note_id]).await;
        } else {
            eprintln!("team note: skipped; team has no notes");
        }
    } else {
        eprintln!("team endpoints: skipped; HACKMD_LIVE_TEST_TEAM_PATH is unset");
    }
}
