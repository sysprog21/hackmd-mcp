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

/// The validator still names the current note.
async fn assert_unchanged(api: &LiveApi, route: &[&str], validator: &str) {
    let response = api.get(route, Some(("if-none-match", validator))).await;
    assert_eq!(response.status, StatusCode::NOT_MODIFIED);
}

/// Saves `title` and `content`, waits until a read shows both, and checks that
/// `old` no longer validates while the new `ETag` does. Returns the new one.
/// `step` names the edit in failure messages.
async fn edit_invalidates(
    api: &LiveApi,
    step: &str,
    route: &[&str],
    old: &str,
    title: &str,
    content: &str,
) -> String {
    api.empty_ok(
        Method::PATCH,
        route,
        Some(&json!({"title": title, "content": content})),
    )
    .await;
    api.poll_json(route, |note| {
        note["title"] == title && note["content"] == content
    })
    .await
    .unwrap_or_else(|| panic!("{step} edit must become visible"));
    let after = api.get(route, Some(("if-none-match", old))).await;
    assert_eq!(
        after.status,
        StatusCode::OK,
        "old ETag must not validate a {step} edit"
    );
    let note = after.value.as_ref().expect("edited note JSON");
    assert_eq!(note["title"], title, "{step} edit title");
    assert_eq!(note["content"], content, "{step} edit content");
    let current = etag(&after);
    // Stated outright rather than left to the 200 and 304 above: the snapshot
    // cache relies on the value itself changing with every edit.
    assert_ne!(current, old, "{step} edit must change the ETag");
    assert_unchanged(api, route, &current).await;
    current
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
        api.poll_json(&route, |note| {
            note["content"] == initial_body && note["title"] == initial_title
        })
        .await
        .expect("initial note must become visible");
        let first = api.get(&route, None).await;
        assert_eq!(first.status, StatusCode::OK);
        let first_note = first.value.as_ref().expect("initial note JSON");
        assert_eq!(first_note["readPermission"], "owner");
        assert_eq!(first_note["writePermission"], "owner");
        let before_body = etag(&first);
        assert_unchanged(api, &route, &before_body).await;
        let changed_body = format!("validator changed {suffix}\n");
        let before_title = edit_invalidates(
            api,
            "body",
            &route,
            &before_body,
            &initial_title,
            &changed_body,
        )
        .await;
        let changed_title = format!("codex-validator-renamed-{suffix}");
        edit_invalidates(
            api,
            "title",
            &route,
            &before_title,
            &changed_title,
            &changed_body,
        )
        .await;
        let workspace = if team.is_some() { "team" } else { "personal" };
        eprintln!(
            "measured validators ({workspace}): body edit old_etag_status=200; \
             title edit old_etag_status=200; unchanged current_etag_status=304"
        );
    })
    .catch_unwind()
    .await;

    // Delete only this new note; no purge request. Team trash placement is
    // unchecked. After a failed probe a failed cleanup is only reported, so
    // the probe's own panic is the one that surfaces.
    if let Err(panic) = outcome {
        api.cleanup_delete(&route, "validator probe note").await;
        std::panic::resume_unwind(panic);
    }
    let deleted = api.request(Method::DELETE, &route, None).await;
    assert!(
        deleted.status.is_success(),
        "validator probe cleanup failed: {}",
        deleted.status
    );
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
