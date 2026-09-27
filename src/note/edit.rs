use serde::Serialize;
use thiserror::Error;

use crate::{
    client::{HackmdClient, HackmdError},
    models::Workspace,
    note::patch::PatchError,
    note::reference::{NoteRefError, NoteResolution},
};

/// What `hackmd_update_note` passes on when given a `patch`.
#[derive(Debug)]
pub(crate) struct EditNoteInput {
    pub(crate) workspace: Workspace,
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
    pub(crate) patch_path: String,
    pub(crate) changed: bool,
    /// Size of the body now stored; `hackmd_get_note` returns the body itself.
    pub(crate) bytes: usize,
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
pub(crate) fn ensure_unchanged(
    note_id: &str,
    body: &str,
    expected: Option<&str>,
) -> Result<(), BodyChanged> {
    let Some(expected) = expected else {
        return Ok(());
    };
    let actual = crate::local::offload(|| crate::sync::state::body_hash(body));
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
    let (_, content) = client.get_note_body(&note.workspace, &note.note_id).await?;
    ensure_unchanged(&note.note_id, &content, input.expected_hash.as_deref())?;
    let patch_path = crate::note::patch::patch_path(&note.workspace, &note.note_id);
    let updated = crate::note::patch::apply_note_patch(&content, &input.patch, &patch_path)?;
    let changed = updated != content;
    if changed {
        client
            .write_note_body(&note.workspace, &note.note_id, &updated)
            .await?;
    }
    Ok(Ok(EditNoteOutput {
        workspace: note.workspace,
        note_id: note.note_id,
        patch_path,
        changed,
        bytes: updated.len(),
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
            workspace: Workspace::Personal,
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
            workspace: Workspace::Personal,
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
        let output = edit_note(&server.client(), input(patch))
            .await
            .expect("edit should succeed")
            .expect("reference should resolve");
        assert!(output.changed);
        assert_eq!(output.bytes, "new\n".len());
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
        stale.expected_hash = Some(crate::sync::state::body_hash("old\n"));
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
        fresh.expected_hash = Some(crate::sync::state::body_hash("old\n"));
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
                workspace: Workspace::Team {
                    team_path: "core".to_owned(),
                },
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
