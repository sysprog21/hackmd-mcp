//! Destructive live API smoke tests.
//!
//! These tests are ignored by default. Run explicitly with a dedicated test
//! account/token and isolated team:
//!
//! ```sh
//! HACKMD_RUN_LIVE_TESTS=1 \
//!     HACKMD_CONFIRM_DESTRUCTIVE_LIVE_TESTS=YES \
//!     HACKMD_LIVE_TEST_TOKEN=... \
//!     HACKMD_LIVE_TEST_TEAM_PATH=... \
//!     cargo test --test live-destructive -- --ignored --nocapture
//! ```

use std::time::{SystemTime, UNIX_EPOCH};

use futures_util::FutureExt;
use reqwest::Method;
use serde_json::{Value, json};

#[path = "support/liveapi.rs"]
#[allow(
    dead_code,
    reason = "shared by live-readonly and live-destructive; each uses a subset"
)]
mod liveapi;

use liveapi::LiveApi;

/// A valid 1x1 RGBA PNG (one red pixel), the smallest image `HackMD` accepts.
const PNG_PROBE: [u8; 70] = [
    137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0, 1, 8, 6, 0,
    0, 0, 31, 21, 196, 137, 0, 0, 0, 13, 73, 68, 65, 84, 8, 215, 99, 248, 207, 192, 240, 31, 0, 5,
    0, 1, 255, 114, 156, 82, 103, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96, 130,
];

/// `PNG_PROBE` grown to `size` bytes by a private ancillary chunk, which a
/// decoder must skip. The file stays a valid PNG, so a refusal measures the
/// size, not the format.
fn padded_png(size: usize) -> Vec<u8> {
    fn crc32(bytes: &[u8]) -> u32 {
        !bytes.iter().fold(u32::MAX, |crc, &byte| {
            (0..8).fold(crc ^ u32::from(byte), |crc, _| {
                (crc >> 1) ^ (0xEDB8_8320 & (crc & 1).wrapping_neg())
            })
        })
    }
    const CHUNK_OVERHEAD: usize = 12;
    // The CRC every PNG ends with, as a check on the one computed here.
    assert_eq!(crc32(b"IEND"), 0xAE42_6082);
    let (head, iend) = PNG_PROBE.split_at(PNG_PROBE.len() - CHUNK_OVERHEAD);
    let padding = size - PNG_PROBE.len() - CHUNK_OVERHEAD;
    let mut chunk = b"prVt".to_vec();
    chunk.resize(4 + padding, 0);
    let mut png = head.to_vec();
    png.extend_from_slice(
        &u32::try_from(padding)
            .expect("padding fits a chunk")
            .to_be_bytes(),
    );
    png.extend_from_slice(&chunk);
    png.extend_from_slice(&crc32(&chunk).to_be_bytes());
    png.extend_from_slice(iend);
    assert_eq!(png.len(), size);
    png
}

#[derive(Default)]
struct Fixtures {
    note: Option<String>,
    child_folder: Option<String>,
    parent_folder: Option<String>,
}

async fn cleanup(api: &LiveApi, fixtures: &mut Fixtures) {
    if let Some(note_id) = fixtures.note.take() {
        api.cleanup_delete(&["notes", &note_id], &format!("note {note_id}"))
            .await;
    }
    if let Some(folder_id) = fixtures.child_folder.take() {
        api.cleanup_delete(
            &["folders", &folder_id],
            &format!("child folder {folder_id}"),
        )
        .await;
    }
    if let Some(folder_id) = fixtures.parent_folder.take() {
        api.cleanup_delete(
            &["folders", &folder_id],
            &format!("parent folder {folder_id}"),
        )
        .await;
    }
}

/// Probe for TODO.md, reported rather than asserted so either answer is
/// recorded; the note's body is restored before returning.
///
/// First half: does a note PATCH that omits content blank the body?
/// `update_note` and create's folder fallback resend the body because it is
/// believed to. The body is judged only once both fields the PATCH set show,
/// since an earlier read could still show the old note and pass for "keeps".
///
/// Second half: patch edits and pushes send content alone and assume every
/// other field survives. A distinct temporary body is sent so the fields are
/// judged only once that PATCH shows; resending the same body could match a
/// read from before it.
async fn probe_partial_note_patch(
    api: &LiveApi,
    note: &[&str],
    body: &str,
    label: &str,
    suffix: u128,
) {
    // A note just created may not show its body yet; judged before it does,
    // creation lag would pass for the PATCH blanking it.
    if api
        .poll_json(note, |read| read["content"] == body)
        .await
        .is_none()
    {
        eprintln!("measured ({label}): note PATCH probes are inconclusive, the body never showed");
        return;
    }
    let marker = format!("probe {suffix}");
    let tags = json!([format!("probe-{suffix}")]);
    api.empty_ok(
        Method::PATCH,
        note,
        Some(&json!({"description": marker, "tags": tags})),
    )
    .await;
    let probed = api
        .poll_json(note, |read| {
            read["description"] == marker.as_str() && read["tags"] == tags
        })
        .await;
    let verdict = match probed.as_ref().map(|read| &read["content"]) {
        None => "is inconclusive, the description and tags never showed",
        Some(content) if *content == body => "keeps the body",
        Some(content) if *content == "" => "blanks the body",
        Some(content) if content.is_null() => "drops the body (null)",
        Some(_) => "changes the body",
    };
    eprintln!(
        "measured ({label}): content-less note PATCH {verdict} (read {:?})",
        probed.as_ref().map(|read| &read["content"])
    );

    // Judged only when the first half confirmed the description and tags were
    // set; otherwise a "clears" would only mean they never landed.
    if probed.is_none() {
        eprintln!(
            "measured ({label}): content-only note PATCH is inconclusive, the description and tags were never set"
        );
    }
    let temporary = format!("{body}\nprobe {suffix}\n");
    api.empty_ok(Method::PATCH, note, Some(&json!({"content": temporary})))
        .await;
    match api
        .poll_json(note, |read| read["content"] == temporary.as_str())
        .await
    {
        None => eprintln!(
            "measured ({label}): content-only note PATCH is inconclusive, the body never showed"
        ),
        Some(_) if probed.is_none() => {}
        Some(read) => {
            for (field, set) in [("description", json!(marker)), ("tags", tags.clone())] {
                eprintln!(
                    "measured ({label}): content-only note PATCH {} {field} (read {})",
                    if read[field] == set {
                        "keeps"
                    } else {
                        "clears"
                    },
                    read[field]
                );
            }
        }
    }

    api.empty_ok(Method::PATCH, note, Some(&json!({"content": body})))
        .await;
    api.poll_json(note, |read| read["content"] == body)
        .await
        .expect("the probe must restore the body");
}

/// Sets one parent's order through the same read-modify-write `child_order`
/// uses, then requires a later read to show it alongside every parent the
/// first read had. `child_order` PUTs back the whole map it reads, so a GET
/// that dropped entries would make it wipe them; this shows the GET returns
/// what was written, not that it returns every entry `HackMD` holds. The
/// original map is put back. Returns how many parents it had, or `None` when
/// that parent already had this order, so the write could show nothing.
async fn check_folder_order(
    api: &LiveApi,
    route: &[&str],
    parent: &str,
    children: &[&str],
) -> Option<usize> {
    let original = api.json(Method::GET, route, None).await;
    let parents = original
        .as_object()
        .unwrap_or_else(|| panic!("folder order must be a map, got {original}"))
        .clone();
    if original[parent] == json!(children) {
        return None;
    }
    let mut seeded = original.clone();
    seeded[parent] = json!(children);
    api.empty_ok(Method::PUT, route, Some(&json!({"order": seeded})))
        .await;

    // The map is the whole account's, so the original goes back before any
    // check can fail, including a read that panics on an HTTP error.
    let read =
        std::panic::AssertUnwindSafe(api.poll_json(route, |read| read[parent] == json!(children)))
            .catch_unwind()
            .await;
    api.empty_ok(Method::PUT, route, Some(&json!({"order": original})))
        .await;
    let read = read
        .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
        .expect("a parent's order just written must read back");
    for (key, value) in &parents {
        if key != parent {
            assert_eq!(&read[key], value, "folder order lost parent {key}");
        }
    }
    Some(parents.len())
}

#[tokio::test]
#[ignore = "requires explicit destructive HackMD live-test environment"]
#[allow(
    clippy::too_many_lines,
    reason = "one linear live workflow keeps fixture creation, assertions, and cleanup ordering auditable"
)]
async fn personal_crud_folder_order_trash_and_restore() {
    let api = LiveApi::destructive_from_env();
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

        // The create above sent no permissions, so this is what HackMD applies
        // by default; hackmd_create_note tells agents not to rely on it.
        eprintln!(
            "measured (personal): a create without permissions reads back read {} / write {}",
            post_read["readPermission"], post_read["writePermission"]
        );

        // The tool allows 5 to 10 MiB with confirmation; is that range real?
        let large = padded_png(7 * 1024 * 1024);
        let status = api
            .upload_png(&["notes", &note_id, "images"], large)
            .await
            .status;
        eprintln!("measured (personal): a 7 MiB image upload answers {status}");

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

        probe_partial_note_patch(&api, &["notes", &note_id], &edited, "personal", suffix).await;

        if check_folder_order(
            &api,
            &["folders", "folder-order"],
            &parent_id,
            &[child_id.as_str()],
        )
        .await
        .is_none()
        {
            eprintln!(
                "measured (personal): folder-order check is inconclusive, the order was already set"
            );
        }

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
async fn team_folder_updates_and_image_upload_are_measured() {
    let api = LiveApi::destructive_from_env();
    let team_path = std::env::var("HACKMD_LIVE_TEST_TEAM_PATH")
        .expect("set an isolated HACKMD_LIVE_TEST_TEAM_PATH for the team probe");
    assert_ne!(team_path.trim(), "");
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

        // The team folder-order route is inferred from the personal one.
        let parents = check_folder_order(
            &api,
            &["teams", team_path.as_str(), "folders", "folder-order"],
            &parent_id,
            &[child_id.as_str()],
        )
        .await;
        eprintln!(
            "measured (team): folder-order {}",
            parents.map_or_else(
                || "check is inconclusive, the order was already set".to_owned(),
                |parents| format!("read-modify-write keeps {parents} parents"),
            )
        );

        // Probe for TODO.md: the update_folder read-back checks only the fields
        // sent, so whether a name-only PATCH keeps the rest is measured here,
        // not assumed. The extras go in their own PATCH so a value HackMD
        // rejects only makes the probe inconclusive.
        let extras = [
            ("description", "probe"),
            ("icon", "1F600"),
            ("color", "#4F46E5"),
        ];
        let folder = ["teams", team_path.as_str(), "folders", child_id.as_str()];
        let shows = |read: &Value, field: &str, value: &str| {
            read[field]
                .as_str()
                .is_some_and(|read| read.eq_ignore_ascii_case(value))
        };
        let extras_set = api
            .request(
                Method::PATCH,
                &folder,
                Some(&Value::Object(
                    extras
                        .iter()
                        .map(|(field, value)| ((*field).to_owned(), json!(value)))
                        .collect(),
                )),
            )
            .await
            .status
            .is_success()
            && api
                .poll_json(&folder, |read| {
                    extras
                        .iter()
                        .all(|(field, value)| shows(read, field, value))
                })
                .await
                .is_some();

        let renamed = format!("codex-live-renamed-{suffix}");
        api.empty_ok(
            Method::PATCH,
            &["teams", &team_path, "folders", &child_id],
            Some(&json!({"name": renamed})),
        )
        .await;
        let updated = api
            .poll_json(&folder, |read| read["name"] == renamed.as_str())
            .await
            .expect("the renamed folder should read back");
        for (field, value) in extras {
            let verdict = if !extras_set {
                "is inconclusive on"
            } else if shows(&updated, field, value) {
                "keeps"
            } else {
                "clears"
            };
            eprintln!(
                "measured (team): name-only folder PATCH {verdict} {field} (read {})",
                updated[field]
            );
        }

        let created = api
            .json(
                Method::POST,
                &["teams", &team_path, "notes"],
                Some(&json!({
                    "title": format!("codex-live-team-image-{suffix}"),
                    "content": "# image probe",
                    "readPermission": "owner",
                    "writePermission": "owner"
                })),
            )
            .await;
        let created_note_id = created["id"].as_str().expect("team note ID").to_owned();
        note_id = Some(created_note_id.clone());

        // hackmd_create_note reports a permission the note does not show; the
        // image privacy check below also depends on this one having taken.
        assert!(
            api.poll_json(&["teams", &team_path, "notes", &created_note_id], |note| {
                note["readPermission"] == "owner" && note["writePermission"] == "owner"
            })
            .await
            .is_some(),
            "a team create dropped the permissions it was sent"
        );
        probe_partial_note_patch(
            &api,
            &["teams", &team_path, "notes", &created_note_id],
            "# image probe",
            "team",
            suffix,
        )
        .await;
        let status = api
            .upload_png(
                &["teams", &team_path, "notes", &created_note_id, "images"],
                PNG_PROBE.to_vec(),
            )
            .await
            .status;
        eprintln!("measured (team): team-prefixed image route answers {status}");

        // The route production uses for every note, team or personal.
        let response = api
            .upload_png(&["notes", &created_note_id, "images"], PNG_PROBE.to_vec())
            .await;
        let (status, body) = (response.status, response.value.unwrap_or_default());
        assert!(
            status.is_success(),
            "plain image route refused a team note ({status}): {body}; \
             hackmd_upload_note_image must refuse team notes again"
        );
        let link = body["data"]["link"]
            .as_str()
            .expect("upload reply should carry data.link");

        // The note is readable only by its owner, so the image must not be. A
        // fresh client carries no token and no cookie.
        let anonymous = reqwest::Client::new()
            .get(link)
            .send()
            .await
            .expect("anonymous image fetch should complete")
            .status();
        eprintln!("measured (team): anonymous fetch of a private note's image answers {anonymous}");
        assert!(
            !anonymous.is_success(),
            "an image on an owner-only team note is publicly readable ({anonymous}); \
             the upload tool must not describe its links as private"
        );
    })
    .catch_unwind()
    .await;

    if let Some(note_id) = note_id {
        api.cleanup_delete(
            &["teams", &team_path, "notes", &note_id],
            &format!("team note {note_id}"),
        )
        .await;
    }
    for folder_id in folder_ids.into_iter().rev() {
        api.cleanup_delete(
            &["teams", &team_path, "folders", &folder_id],
            &format!("team folder {folder_id}"),
        )
        .await;
    }
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}
