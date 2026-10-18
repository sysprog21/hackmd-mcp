use serde::Serialize;
use thiserror::Error;

use crate::{
    client::{HackmdClient, HackmdError},
    models::Workspace,
    note::body::{body_title, into_stored},
    note::patch::PatchError,
    note::reference::{NoteRefError, NoteResolution},
};

/// What `hackmd_update_note` passes on when given a `patch`.
#[derive(Debug)]
pub(crate) struct EditNoteInput {
    pub(crate) workspace: Option<Workspace>,
    pub(crate) note_ref: String,
    pub(crate) refresh: bool,
    pub(crate) patch: String,
    /// The `body_hash` the patch was written against, if the caller gave one.
    pub(crate) expected_hash: Option<String>,
}

#[derive(Debug, Serialize)]
pub(crate) struct EditNoteOutput {
    #[serde(rename = "team_path")]
    pub(crate) workspace: Workspace,
    pub(crate) note_id: String,
    /// See [`crate::note::get::note_url`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) note_url: Option<String>,
    pub(crate) patch_path: String,
    pub(crate) changed: bool,
    /// Size of the body now stored; `hackmd_get_note` returns the body itself.
    pub(crate) bytes: usize,
    /// The title the patched body implies when it differs from the listed
    /// one, as `hackmd_push_note` reports it: a patch that renames the H1
    /// leaves the listing unchanged, so set `title` in a separate update.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) title_drift: Option<String>,
}

#[derive(Debug, Error)]
pub(crate) enum EditNoteError {
    #[error(transparent)]
    Reference(#[from] NoteRefError),
    #[error(transparent)]
    Api(#[from] HackmdError),
    #[error(transparent)]
    Patch(#[from] PatchError),
    #[error(transparent)]
    BodyChanged(#[from] BodyChanged),
}

/// The body is no longer the one the caller read.
#[derive(Debug, Error)]
#[error(
    "note {note_id} changed since it was read (body hash {actual}, expected {expected}); get it again and rebuild the change"
)]
pub(crate) struct BodyChanged {
    pub(crate) note_id: String,
    pub(crate) expected: String,
    pub(crate) actual: String,
}

/// Refuses a body write when `expected` is given and the current body does
/// not hash to it. `HackMD` has no conditional write, so a change landing
/// between this read and the write still wins; this catches every change
/// made before the read, which is where a stale agent's edits come from.
/// `digest` is the body's, asked for only when there is a hash to check.
pub(crate) fn ensure_unchanged(
    note_id: &str,
    digest: impl FnOnce() -> [u8; 32],
    expected: Option<&str>,
) -> Result<(), BodyChanged> {
    let Some(expected) = expected else {
        return Ok(());
    };
    let actual = crate::hash::body_hash_from_digest(&digest());
    if actual == expected {
        Ok(())
    } else {
        Err(BodyChanged {
            note_id: note_id.to_owned(),
            expected: expected.to_owned(),
            actual,
        })
    }
}

impl crate::reply::ToolError for EditNoteError {
    fn kind(&self) -> crate::reply::ErrorKind {
        match self {
            Self::Reference(error) => error.kind(),
            Self::Api(error) => error.kind(),
            Self::Patch(error) => error.kind(),
            Self::BodyChanged(_) => crate::reply::ErrorKind::Conflict,
        }
    }
}

pub(crate) async fn edit_note(
    client: &HackmdClient,
    input: EditNoteInput,
) -> Result<Result<EditNoteOutput, NoteResolution>, EditNoteError> {
    let resolution = crate::note::reference::resolve_note_ref(
        client,
        input.workspace,
        &input.note_ref,
        input.refresh,
    )
    .await?;
    let NoteResolution::Resolved { note } = resolution else {
        return Ok(Err(resolution));
    };
    let current = client
        .get_note_snapshot(&note.workspace, &note.note_id)
        .await?;
    ensure_unchanged(
        &note.note_id,
        || current.digest(),
        input.expected_hash.as_deref(),
    )?;
    let content = std::sync::Arc::clone(&current.content);
    drop(current);
    let patch_path = crate::note::patch::patch_path(&note.workspace, &note.note_id);
    // Matching hunks against a large body is CPU work, kept off the runtime.
    let (updated, changed, synced_title) = crate::local::offload(|| {
        // An added line may carry a lone `\r`; HackMD would store it as `\n`.
        let updated = crate::note::patch::apply_note_patch(&content, &input.patch, &patch_path)
            .map(into_stored)?;
        let changed = updated.as_str() != content.as_str();

        // The body before the patch stands in for the last-synced one: drift is
        // reported only when its title matched the listing.
        let synced_title = changed.then(|| body_title(&content)).flatten();
        Ok::<_, PatchError>((updated, changed, synced_title))
    })?;

    // The old body is not needed past here; a large note is held once less
    // through the write and its read-back.
    drop(content);
    let mut title_drift = None;
    if changed {
        // Judged against the read-back's title, as push does, so a rename made
        // meanwhile is respected. Only the title is kept: the read-back body is
        // another full copy of the note.
        let listed = client
            .write_note_body(&note.workspace, &note.note_id, &updated)
            .await?
            .title;
        title_drift = crate::local::offload(|| {
            crate::note::body::title_drift(&listed, &updated, || synced_title)
        });
    }
    Ok(Ok(EditNoteOutput {
        workspace: note.workspace,
        note_url: crate::note::get::note_url(client.site_url(), &note.note_id),
        note_id: note.note_id,
        patch_path,
        changed,
        bytes: updated.len(),
        title_drift,
    }))
}

#[cfg(test)]
mod tests {
    use super::{EditNoteError, EditNoteInput, edit_note};
    use crate::{
        client::HackmdError,
        fixture::{Scenario, SequenceServer},
        models::Workspace,
        note::patch::PatchError,
    };

    fn input(patch: &str) -> EditNoteInput {
        EditNoteInput {
            workspace: None,
            note_ref: "note-id".to_owned(),
            refresh: false,
            patch: patch.to_owned(),
            expected_hash: None,
        }
    }

    #[tokio::test]
    async fn a_write_that_never_becomes_visible_is_an_error() {
        const OLD: &str = r#"{"id":"note-id","title":"Title","content":"old"}"#;

        // Fetch, PATCH, then a read-back that keeps showing the old body.
        let fixture = SequenceServer::spawn_repeating([(200, OLD), (202, ""), (200, OLD)]);
        let client = fixture.client();
        let input = EditNoteInput {
            workspace: None,
            note_ref: "note-id".to_owned(),
            refresh: false,
            patch:
                "*** Begin Patch\n*** Update File: notes/note-id.md\n@@\n-old\n+new\n*** End Patch"
                    .to_owned(),
            expected_hash: None,
        };

        assert!(matches!(
            edit_note(&client, input).await,
            Err(EditNoteError::Api(HackmdError::ReadbackMismatch { .. }))
        ));
    }

    #[tokio::test]
    async fn a_patched_lone_cr_is_written_as_lf() {
        // `str::lines` keeps a `\r` inside a line; HackMD would store `\n`, so
        // the body as patched could never be confirmed.
        let server = SequenceServer::spawn_scenarios([
            Scenario::new(
                "GET",
                "/v1/notes/note-id",
                200,
                r#"{"id":"note-id","title":"Title","content":"old\n"}"#,
            ),
            Scenario::new("PATCH", "/v1/notes/note-id", 202, "")
                .expect_body("LF line endings", |body| body == r#"{"content":"a\nb\n"}"#),
            Scenario::new(
                "GET",
                "/v1/notes/note-id",
                200,
                r#"{"id":"note-id","title":"Title","content":"a\nb\n"}"#,
            ),
        ]);
        let patch =
            "*** Begin Patch\n*** Update File: notes/note-id.md\n@@\n-old\n+a\rb\n*** End Patch";
        edit_note(&server.client(), input(patch))
            .await
            .expect("edit should be confirmed")
            .expect("direct reference should resolve");
        server.finish();
    }

    #[tokio::test]
    async fn changed_edit_gets_then_patches_full_updated_content() {
        let server = SequenceServer::spawn_scenarios([
            Scenario::new(
                "GET",
                "/v1/notes/note-id",
                200,
                r#"{"id":"note-id","title":"Title","content":"old\n"}"#,
            ),
            Scenario::new("PATCH", "/v1/notes/note-id", 202, "")
                .expect_body("the complete updated content", |body| {
                    body == r#"{"content":"new\n"}"#
                }),
            Scenario::new(
                "GET",
                "/v1/notes/note-id",
                200,
                r#"{"id":"note-id","title":"Title","content":"new\n"}"#,
            ),
        ]);
        let patch =
            "*** Begin Patch\n*** Update File: notes/note-id.md\n@@\n-old\n+new\n*** End Patch";
        let output = edit_note(&server.client_with_site(), input(patch))
            .await
            .expect("edit should succeed")
            .expect("reference should resolve");
        assert!(output.changed);
        assert_eq!(output.bytes, "new\n".len());
        assert_eq!(
            output.note_url.as_deref(),
            Some("https://hackmd.io/note-id")
        );
        server.finish();
    }

    /// One patch edit: the note as read, then the PATCH and its read-back.
    async fn patch_once(title: &str, old: &str, patch_lines: &str) -> super::EditNoteOutput {
        patch_with_readback(title, title, old, patch_lines).await
    }

    /// As [`patch_once`], with the read-back listing `read_back_title`.
    async fn patch_with_readback(
        title: &str,
        read_back_title: &str,
        old: &str,
        patch_lines: &str,
    ) -> super::EditNoteOutput {
        let patch = format!(
            "*** Begin Patch\n*** Update File: notes/note-id.md\n{patch_lines}*** End Patch"
        );
        let new = crate::note::patch::apply_note_patch(old, &patch, "notes/note-id.md")
            .expect("the test patch applies");
        let note = |title: &str, content: &str| {
            serde_json::json!({"id": "note-id", "title": title, "content": content}).to_string()
        };
        let server = SequenceServer::spawn_scenarios([
            Scenario::new("GET", "/v1/notes/note-id", 200, &note(title, old)),
            Scenario::new("PATCH", "/v1/notes/note-id", 202, ""),
            Scenario::new(
                "GET",
                "/v1/notes/note-id",
                200,
                &note(read_back_title, &new),
            ),
        ]);
        let output = edit_note(&server.client(), input(&patch))
            .await
            .expect("edit should succeed")
            .expect("reference should resolve");
        server.finish();
        output
    }

    #[tokio::test]
    async fn a_patch_that_renames_the_h1_reports_title_drift() {
        let output = patch_once(
            "Old name",
            "# Old name\n\nbody\n",
            "@@\n-# Old name\n+# New name\n",
        )
        .await;
        assert_eq!(output.title_drift.as_deref(), Some("New name"));
    }

    #[tokio::test]
    async fn drift_follows_a_rename_made_during_the_patch() {
        // The listing already shows the new H1 when the write is read back.
        let output = patch_with_readback(
            "Old name",
            "New name",
            "# Old name\n\nbody\n",
            "@@\n-# Old name\n+# New name\n",
        )
        .await;
        assert_eq!(output.title_drift, None);
    }

    #[tokio::test]
    async fn a_patch_that_leaves_the_h1_reports_no_drift() {
        let output = patch_once("Old name", "# Old name\n\nbody\n", "@@\n-body\n+text\n").await;
        assert_eq!(output.title_drift, None);
    }

    #[tokio::test]
    async fn a_heading_that_never_matched_the_listing_is_not_drift() {
        // A listed title set apart from the H1 on purpose stays silent, as in
        // push.
        let output = patch_once(
            "Listed",
            "# Heading\n\nbody\n",
            "@@\n-# Heading\n+# Renamed\n",
        )
        .await;
        assert_eq!(output.title_drift, None);
    }

    #[tokio::test]
    async fn a_patch_that_retitles_the_front_matter_reports_title_drift() {
        let output = patch_once(
            "Old name",
            "---\ntitle: Old name\n---\nbody\n",
            "@@\n-title: Old name\n+title: New name\n",
        )
        .await;
        assert_eq!(output.title_drift.as_deref(), Some("New name"));
    }

    #[tokio::test]
    async fn a_first_h1_added_by_a_patch_is_not_drift() {
        // The body gave no title before, so nothing says the listing followed
        // it.
        let output = patch_once("Listed", "body\n", "@@\n+# Heading\n body\n").await;
        assert_eq!(output.title_drift, None);
    }

    #[tokio::test]
    async fn a_patch_that_changes_nothing_writes_nothing_and_reports_no_drift() {
        let server = SequenceServer::spawn_scenarios([Scenario::new(
            "GET",
            "/v1/notes/note-id",
            200,
            r##"{"id":"note-id","title":"Listed","content":"# Other\n"}"##,
        )]);
        let output = edit_note(
            &server.client(),
            input("*** Begin Patch\n*** Update File: notes/note-id.md\n@@\n-# Other\n+# Other\n*** End Patch"),
        )
        .await
        .expect("edit should succeed")
        .expect("reference should resolve");
        assert!(!output.changed);
        assert_eq!(output.title_drift, None);
        server.finish();
    }

    #[tokio::test]
    async fn an_expected_hash_refuses_a_body_that_changed_since_it_was_read() {
        let server = SequenceServer::spawn_scenarios([Scenario::new(
            "GET",
            "/v1/notes/note-id",
            200,
            r#"{"id":"note-id","title":"Title","content":"old\nbrowser\n"}"#,
        )]);
        let mut stale = input(
            "*** Begin Patch\n*** Update File: notes/note-id.md\n@@\n-old\n+new\n*** End Patch",
        );
        stale.expected_hash = Some(crate::hash::body_hash("old\n"));
        let error = edit_note(&server.client(), stale)
            .await
            .expect_err("a changed body must not be written");
        assert!(matches!(error, EditNoteError::BodyChanged(_)));
        assert!(
            error
                .to_string()
                .starts_with("note note-id changed since it was read")
        );
        assert_eq!(server.finish().len(), 1, "nothing may be written");
    }

    #[tokio::test]
    async fn a_matching_expected_hash_lets_the_patch_through() {
        let server = SequenceServer::spawn_scenarios([
            Scenario::new(
                "GET",
                "/v1/notes/note-id",
                200,
                r#"{"id":"note-id","title":"Title","content":"old\n"}"#,
            ),
            Scenario::new("PATCH", "/v1/notes/note-id", 202, ""),
            Scenario::new(
                "GET",
                "/v1/notes/note-id",
                200,
                r#"{"id":"note-id","title":"Title","content":"new\n"}"#,
            ),
        ]);
        let mut fresh = input(
            "*** Begin Patch\n*** Update File: notes/note-id.md\n@@\n-old\n+new\n*** End Patch",
        );
        fresh.expected_hash = Some(crate::hash::body_hash("old\n"));
        let output = edit_note(&server.client(), fresh)
            .await
            .expect("an unchanged body should be patched")
            .expect("reference should resolve");
        assert!(output.changed);
        server.finish();
    }

    #[tokio::test]
    async fn no_op_edit_gets_once_and_never_patches() {
        let server = SequenceServer::spawn_scenarios([Scenario::new(
            "GET",
            "/v1/notes/note-id",
            200,
            r#"{"id":"note-id","title":"Title","content":"same"}"#,
        )]);
        let patch = "*** Begin Patch\n*** Update File: notes/note-id.md\n@@\n same\n*** End Patch";
        let output = edit_note(&server.client(), input(patch))
            .await
            .expect("no-op should succeed")
            .expect("reference should resolve");
        assert!(!output.changed);
        server.finish();
    }

    #[tokio::test]
    async fn patch_conflict_returns_distinct_error_without_patch_request() {
        let server = SequenceServer::spawn_scenarios([Scenario::new(
            "GET",
            "/v1/notes/note-id",
            200,
            r#"{"id":"note-id","title":"Title","content":"actual"}"#,
        )]);
        let patch =
            "*** Begin Patch\n*** Update File: notes/note-id.md\n@@\n-missing\n+new\n*** End Patch";
        let error = edit_note(&server.client(), input(patch))
            .await
            .expect_err("missing context should conflict");
        assert!(matches!(
            error,
            EditNoteError::Patch(PatchError::ContextNotFound)
        ));
        assert_eq!(server.finish().len(), 1);
    }

    #[tokio::test]
    async fn wrong_target_and_ambiguous_context_also_never_patch() {
        let cases = [
            (
                r#"{"id":"note-id","title":"Title","content":"old"}"#,
                "*** Begin Patch\n*** Update File: notes/other.md\n@@\n-old\n+new\n*** End Patch",
                "patch targets notes/other.md, expected notes/note-id.md",
            ),
            (
                r#"{"id":"note-id","title":"Title","content":"repeat\nrepeat"}"#,
                "*** Begin Patch\n*** Update File: notes/note-id.md\n@@\n-repeat\n+new\n*** End Patch",
                "patch hunk context matched multiple locations",
            ),
        ];
        for (body, patch, expected) in cases {
            let server = SequenceServer::spawn_scenarios([Scenario::new(
                "GET",
                "/v1/notes/note-id",
                200,
                body,
            )]);
            let message = edit_note(&server.client(), input(patch))
                .await
                .expect_err("patch must conflict")
                .to_string();
            assert_eq!(message, expected);
            assert_eq!(server.finish().len(), 1);
        }
    }

    #[tokio::test]
    async fn team_edit_uses_team_item_route_and_team_patch_path() {
        let server = SequenceServer::spawn_scenarios([
            Scenario::new(
                "GET",
                "/v1/teams/core/notes/note-id",
                200,
                r#"{"id":"note-id","title":"Title","content":"old"}"#,
            ),
            Scenario::new("PATCH", "/v1/teams/core/notes/note-id", 202, ""),
            Scenario::new(
                "GET",
                "/v1/teams/core/notes/note-id",
                200,
                r#"{"id":"note-id","title":"Title","content":"new"}"#,
            ),
        ]);
        let patch = "*** Begin Patch\n*** Update File: teams/core/notes/note-id.md\n@@\n-old\n+new\n*** End Patch";
        let output = edit_note(
            &server.client(),
            EditNoteInput {
                workspace: Some(Workspace::Team {
                    team_path: "core".to_owned(),
                }),
                note_ref: "note-id".to_owned(),
                refresh: false,
                patch: patch.to_owned(),
                expected_hash: None,
            },
        )
        .await
        .expect("team edit should succeed")
        .expect("reference should resolve");
        assert_eq!(output.patch_path, "teams/core/notes/note-id.md");
        server.finish();
    }
}
