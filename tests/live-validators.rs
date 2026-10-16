//! Isolated write probe for note `ETags`; never touches existing notes.
//! Run only after explicit write authorization with the destructive live gates:
//! cargo test --test live-validators -- --ignored --nocapture
use futures_util::FutureExt;
use reqwest::{Method, StatusCode};
use serde_json::json;

#[path = "support/liveapi.rs"]
#[allow(
    dead_code,
    reason = "shared live helper; this probe uses only a subset"
)]
mod liveapi;
use liveapi::{LiveApi, LiveResponse};

fn etag(response: &LiveResponse) -> String {
    response
        .headers
        .get("etag")
        .and_then(|value| value.to_str().ok())
        .expect("note GET must provide an ETag")
        .to_owned()
}

async fn probe_workspace(api: &LiveApi, team: Option<&str>, suffix: u128) {
    let mut route = team.map_or_else(Vec::new, |team| vec!["teams", team]);
    route.push("notes");
    let initial_title = format!("codex-validator-{suffix}");
    let initial_body = format!("validator initial {suffix}\n");
    let created = api
        .json(
            Method::POST,
            &route,
            Some(&json!({
                "title": initial_title, "content": initial_body,
                "readPermission": "owner", "writePermission": "owner",
            })),
        )
        .await;
    let id = created["id"]
        .as_str()
        .expect("created private note ID")
        .to_owned();
    route.push(&id);
    let outcome = std::panic::AssertUnwindSafe(async {
        api.poll_json(&route, |note| note["content"] == initial_body && note["title"] == initial_title)
            .await.expect("initial note must become visible");
        let first = api.get(&route, None).await;
        assert_eq!(first.status, StatusCode::OK);
        let first_note = first.value.as_ref().expect("initial note JSON");
        assert_eq!(first_note["readPermission"], "owner");
        assert_eq!(first_note["writePermission"], "owner");
        let before_body = etag(&first);
        assert_eq!(api.get(&route, Some(("if-none-match", &before_body))).await.status, StatusCode::NOT_MODIFIED);
        let changed_body = format!("validator changed {suffix}\n");
        api.empty_ok(Method::PATCH, &route, Some(&json!({"title": initial_title, "content": changed_body}))).await;
        api.poll_json(&route, |note| note["content"] == changed_body).await.expect("body edit must become visible");
        let after_body = api.get(&route, Some(("if-none-match", &before_body))).await;
        assert_eq!(after_body.status, StatusCode::OK, "old ETag must not validate an edited body");
        assert_eq!(after_body.value.as_ref().unwrap()["content"], changed_body);
        let before_title = etag(&after_body);
        assert_ne!(before_title, before_body);
        assert_eq!(api.get(&route, Some(("if-none-match", &before_title))).await.status, StatusCode::NOT_MODIFIED);
        let changed_title = format!("codex-validator-renamed-{suffix}");
        api.empty_ok(Method::PATCH, &route, Some(&json!({"title": changed_title, "content": changed_body}))).await;
        api.poll_json(&route, |note| note["title"] == changed_title && note["content"] == changed_body)
            .await.expect("title edit must become visible with unchanged body");
        let after_title = api.get(&route, Some(("if-none-match", &before_title))).await;
        assert_eq!(after_title.status, StatusCode::OK, "old ETag must not validate an edited title");
        assert_eq!(after_title.value.as_ref().unwrap()["title"], changed_title);
        assert_eq!(after_title.value.as_ref().unwrap()["content"], changed_body);
        let current = etag(&after_title);
        assert_ne!(current, before_title);
        assert_eq!(api.get(&route, Some(("if-none-match", &current))).await.status, StatusCode::NOT_MODIFIED);
        eprintln!("measured validators ({}): body edit old_etag_status=200; title edit old_etag_status=200; unchanged current_etag_status=304", if team.is_some() {"team"} else {"personal"});
    }).catch_unwind().await;

    // Delete only this new note; no purge request. Team trash placement is
    // unchecked.
    let deleted = api.request(Method::DELETE, &route, None).await;
    assert!(
        deleted.status.is_success(),
        "validator probe cleanup failed: {}",
        deleted.status
    );
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}

#[tokio::test]
#[ignore = "requires explicit authorization to create, edit and delete two new probe notes"]
async fn body_and_title_edits_invalidate_personal_and_team_etags() {
    let api = LiveApi::destructive_from_env();
    let team = std::env::var("HACKMD_LIVE_TEST_TEAM_PATH").expect("set an authorized team path");
    let team = team.trim();
    assert_ne!(team, "");
    let suffix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    probe_workspace(&api, None, suffix).await;
    probe_workspace(&api, Some(team), suffix).await;
}
