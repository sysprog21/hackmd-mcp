//! Read-only live `HackMD` contract and conditional-request probe.
//!
//! This test never creates, edits, or deletes data. Run it manually with a
//! dedicated token and preserve `--nocapture` output as the validator evidence.
//! The large note belongs to the selected team (personal when the team path
//! is unset) and has at least 64 KiB of Markdown.
//!
//! ```sh
//! HACKMD_RUN_LIVE_READONLY_TESTS=1 \
//!     HACKMD_LIVE_TEST_TOKEN=... \
//!     HACKMD_LIVE_TEST_TEAM_PATH=... \
//!     HACKMD_LIVE_TEST_LARGE_NOTE_ID=... \
//!     cargo test --test live-readonly -- --ignored --nocapture
//! ```

use reqwest::{StatusCode, header::HeaderMap};
use serde_json::Value;

#[path = "support/liveapi.rs"]
#[allow(
    dead_code,
    reason = "shared by live-readonly and live-destructive; each uses a subset"
)]
mod liveapi;

use liveapi::{LiveApi, LiveResponse};

fn header_text<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|value| value.to_str().ok())
}

async fn probe_endpoint(api: &LiveApi, label: &str, segments: &[&str]) -> LiveResponse {
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

    let validators = [
        etag.map(|value| ("if-none-match", value)),
        modified.map(|value| ("if-modified-since", value)),
    ];
    for validator in validators.into_iter().flatten() {
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
    }
    if etag.is_none() && modified.is_none() {
        eprintln!("{label}: conditional probe skipped; no validator header");
    }
    first
}

#[tokio::test]
#[ignore = "requires explicit read-only HackMD live-test environment and a large note"]
async fn a_large_note_get_reports_compression_and_transferred_bytes() {
    let api = LiveApi::readonly_from_env();
    let note_id = std::env::var("HACKMD_LIVE_TEST_LARGE_NOTE_ID")
        .expect("set HACKMD_LIVE_TEST_LARGE_NOTE_ID to an existing note in the selected workspace with at least 64 KiB of Markdown");
    assert!(
        !note_id.trim().is_empty(),
        "large note ID must not be empty"
    );
    let team_path = std::env::var("HACKMD_LIVE_TEST_TEAM_PATH").ok();
    let segments = match team_path.as_deref() {
        Some(team) => {
            assert!(!team.trim().is_empty(), "team path must not be empty");
            vec!["teams", team, "notes", note_id.as_str()]
        }
        None => vec!["notes", note_id.as_str()],
    };
    let plain = api
        .get(&segments, Some(("accept-encoding", "identity")))
        .await;
    assert!(
        plain.status.is_success(),
        "large note returned {}",
        plain.status
    );
    let identity_encoding = header_text(&plain.headers, "content-encoding");
    eprintln!(
        "compression identity: content_encoding={identity_encoding:?} transferred_body_bytes={}",
        plain.body_len
    );
    assert!(
        identity_encoding.is_none_or(|encoding| encoding.eq_ignore_ascii_case("identity")),
        "server ignored Accept-Encoding: identity"
    );
    let markdown_bytes = plain
        .value
        .as_ref()
        .and_then(|value| value["content"].as_str())
        .expect("identity note GET must contain JSON Markdown content")
        .len();
    assert!(
        markdown_bytes >= 64 * 1024,
        "large note has {markdown_bytes} Markdown bytes; at least 64 KiB is required"
    );
    for accept in ["gzip, br", "gzip"] {
        let packed = api.get(&segments, Some(("accept-encoding", accept))).await;
        assert!(
            packed.status.is_success(),
            "compressed note GET returned {}",
            packed.status
        );
        eprintln!(
            "compression: accept_encoding={accept:?} markdown_bytes={markdown_bytes} identity_body_bytes={} content_encoding={:?} transferred_body_bytes={}",
            plain.body_len,
            header_text(&packed.headers, "content-encoding"),
            packed.body_len,
        );
    }
}

#[tokio::test]
#[ignore = "requires explicit read-only HackMD live-test environment"]
async fn personal_team_and_note_gets_report_validator_support() {
    let api = LiveApi::readonly_from_env();
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
